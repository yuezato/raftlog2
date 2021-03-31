use futures::{Async, Future};
use std::collections::BTreeMap;
use std::mem;
use trackable::error::ErrorKindExt;

use super::super::Common;
use crate::cluster::ClusterConfig;
use crate::log::{Log, LogIndex};
use crate::message::{AppendEntriesReply, SequenceNumber};
use crate::node::NodeId;
use crate::{ErrorKind, Io, Result};

/// 各Followerの管理に使う情報
#[derive(Debug)]
struct Follower {
    pub obsolete_seq_no: SequenceNumber,

    pub log_tail: LogIndex,
    pub last_seq_no: SequenceNumber,
    pub synced: bool,
}
impl Follower {
    pub fn new() -> Self {
        Follower {
            obsolete_seq_no: SequenceNumber::new(0),

            log_tail: LogIndex::new(0),
            last_seq_no: SequenceNumber::new(0),
            synced: false,
        }
    }
}

/// フォロワーの管理者.
///
/// フォロワー一覧と、それぞれのローカルログの状態の把握が主責務.
/// フォロワーのローカルログがリーダのものよりも遅れている場合には、
/// その同期(差分送信)も実施する.
pub struct FollowersManager<IO: Io> {
    followers: BTreeMap<NodeId, Follower>,
    config: ClusterConfig,
    latest_hearbeat_ack: SequenceNumber,
    last_broadcast_seq_no: SequenceNumber,

    // `raft_test_simu`のために非決定的な要素は排除したいので、
    // `HashMap`ではなく`BTreeMap`を使用している. <- ここに書くことではない
    // 
    // 
    tasks: BTreeMap<NodeId, IO::LoadLog>,
}
impl<IO: Io> FollowersManager<IO> {
    pub fn new(config: ClusterConfig) -> Self {
        let followers = config
            .members()
            .map(|n| (n.clone(), Follower::new()))
            .collect();
        FollowersManager {
            followers,
            config,
            tasks: BTreeMap::new(),
            latest_hearbeat_ack: SequenceNumber::new(0),
            last_broadcast_seq_no: SequenceNumber::new(0),
        }
    }
    pub fn run_once(&mut self, common: &mut Common<IO>) -> Result<()> {
        // バックグランドタスク(ログ同期用の読み込み処理)を実行する.
        
        // dones: 完了したログ読み込み処理の結果を入れる
        let mut dones: Vec<(NodeId, Log)> = Vec::new();
        for (follower, task) in &mut self.tasks {
            if let Async::Ready(log) = track!(task.poll())? {
                dones.push((follower.clone(), log));
            }
        }
        
        for (follower, log) in dones {
            let rpc = common.rpc_caller();
            match log {
                Log::Prefix(snapshot) => rpc.send_install_snapshot(&follower, snapshot),
                Log::Suffix(slice) => rpc.send_append_entries(&follower, slice),
            }
            self.tasks.remove(&follower);
        }
        Ok(())
    }
    
    pub fn latest_hearbeat_ack(&self) -> SequenceNumber {
        self.latest_hearbeat_ack
    }

    /// コミット済みログ領域の終端を返す.
    ///
    /// "コミット済み"とは「投票権を有するメンバの過半数以上のローカルログに存在する」ということを意味する.
    /// (構成変更中で、新旧構成の両方に投票権が存在する場合には、そのそれぞれの過半数以上)
    pub fn committed_log_tail(&self) -> LogIndex {
        self.config.consensus_value(|node_id| {
            let f = &self.followers[node_id];
            if f.synced {
                f.log_tail
            } else {
                LogIndex::new(0)
            }
        })
    }

    /// ジョイントコミット済みのログ領域の終端を返す.
    ///
    /// 基本的には`committed_log_tail`と同じ動作となるが、
    /// 「構成変更中」かつ「`ClusterState`の値が`CatchUp`」の場合でも、
    /// こちらの関数は常に新旧両方から「過半数以上」を要求する点が異なる.
    pub fn joint_committed_log_tail(&self) -> LogIndex {
        self.config.full_consensus_value(|node_id| {
            let f = &self.followers[node_id];
            if f.synced {
                f.log_tail
            } else {
                LogIndex::new(0)
            }
        })
    }

    pub fn handle_append_entries_reply(
        &mut self,
        common: &Common<IO>,
        reply: &AppendEntriesReply,
    ) -> bool {
        let updated = self.update_follower_state(common, reply);
        if self.latest_hearbeat_ack < reply.header.seq_no {
            self.latest_hearbeat_ack = self
                .config
                .consensus_value(|node_id| self.followers[node_id].last_seq_no);
        }
        updated
    }

    pub fn set_last_broadcast_seq_no(&mut self, seq_no: SequenceNumber) {
        self.last_broadcast_seq_no = seq_no;
    }

    /// フォロワーのローカルログとの同期処理を実行する.
    pub fn log_sync(&mut self, common: &mut Common<IO>, reply: &AppendEntriesReply) -> Result<()> {
        if reply.busy || self.tasks.contains_key(&reply.header.sender) {
            // フォロワーが忙しい or 既に同期処理が進行中
            return Ok(());
        }

        let follower = track!(
            self
                .followers
                .get_mut(&reply.header.sender)
                .ok_or_else(|| ErrorKind::InconsistentState.error())
        )?;
        
        if reply.header.seq_no <= follower.obsolete_seq_no {
            // 平行度が高くなりすぎるのを防止するために、
            // propose(broadcast)が重なった場合には、
            // `obsolete_seq_no`以前のbroadcastに対する応答は古いものとして処理を省く.
            return Ok(());
        }
        follower.obsolete_seq_no = self.last_broadcast_seq_no;

        if common.log().tail().index <= follower.log_tail {
            // The follower is up-to-date
            return Ok(());
        }

        // syncedのtrue/falseの切り替わりは細かく調べておく
        let end = if follower.synced {
            // フォロワーのログとリーダのログの差分を送信
            common.log().tail().index
        } else {
            // フォロワーのログとリーダのログの同期(合流)点を探索中
            // 意味不明
            follower.log_tail
        };

        // end == follower.log_tail の場合には何を計算することになるんだ
        // https://github.com/frugalos/frugalos/blob/develop/frugalos_raft/src/storage/mod.rs#L112
        // これを読むと何をしている??
        // raw logの範疇の場合は空のrawlogっぽい
        // その場合って何が起こるんだ??
        // update_follower_state では synced=false で -1 してるので
        // もう一回heart beatしている??
        //
        // でsnapshotの中にある場合は空のlogではなく
        // snapshotを返しているような気がする。これ結構問題じゃないかなあ
        let future = common.load_log(follower.log_tail, Some(end));
        self.tasks.insert(reply.header.sender.clone(), future);
        Ok(())
    }

    /// クラスタ構成の変更に追従する.
    pub fn handle_config_updated(&mut self, config: &ClusterConfig) {
        // Add
        for id in config.members() {
            if !self.followers.contains_key(id) {
                self.followers.insert(id.clone(), Follower::new());
            }
        }

        // Delete
        self.followers = mem::replace(&mut self.followers, BTreeMap::new())
            .into_iter()
            .filter(|&(ref id, _)| config.is_known_node(id))
            .collect();

        self.config = config.clone();
    }

    // `reply`を調べて
    // どのfollowerから帰ってきたかを突き止めた上で
    // そのfollowerに対するこれまでの知識を
    // 更新する必要がある(true)かない(false)かを判定する
    fn update_follower_state(&mut self, common: &Common<IO>, reply: &AppendEntriesReply) -> bool {
        // どのfollowerから来たか
        let who_send = &reply.header.sender;
        
        // 現時点でleaderがfollower `who_send` について知っていること
        let follower = &mut self
            .followers
            .get_mut(who_send)
            .expect("Never fails");
        if follower.last_seq_no < reply.header.seq_no {
            follower.last_seq_no = reply.header.seq_no;
        }
        
        match *reply {
            AppendEntriesReply { busy: true, .. } => false,
            AppendEntriesReply { log_tail, .. } if follower.synced => {
                let updated = follower.log_tail < log_tail.index;
                if updated {
                    follower.log_tail = log_tail.index;
                } else if log_tail.index.as_u64() == 0 && follower.log_tail.as_u64() != 0 {
                    // NOTE: followerのデータがクリアされたものと判断する
                    // FIXME: ちゃんとした実装にする(e.g., ノードに再起動毎に替わるようなIDを付与して、その一致を確認する)
                    follower.synced = false;
                }
                updated
            }
            AppendEntriesReply { log_tail, .. } => {
                // Noopしか入ってないところで
                // prevを取ると
                // 一つ前のTermが出てこないか??
                let leader_term = common
                    .log()
                    .get_record(log_tail.index)
                    .map(|r| r.head.prev_term);
                
                follower.synced = leader_term == Some(log_tail.prev_term);
                if follower.synced {
                    follower.log_tail = log_tail.index;
                } else {
                    follower.log_tail = log_tail.index.as_u64().saturating_sub(1).into();
                }

                println!("<update_follower_state>");
                dbg!(who_send);
                dbg!(&follower);
                println!("</update_follower_state>");
                
                // updatedと意味的に一致しているか??
                follower.synced
            }
        }
    }
}
