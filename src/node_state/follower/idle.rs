use std::marker::PhantomData;
use trackable::error::ErrorKindExt;

use super::super::{Common, NextState, RoleState};
use super::{Follower, FollowerAppend, FollowerSnapshot};
use crate::log::{LogPosition, LogSuffix};
use crate::message::{AppendEntriesCall, Message};
use crate::{ErrorKind, Io, Result};

/// 待機中(i.e., 受信メッセージ処理が可能)なフォロワーのサブ状態.
///
/// リーダから送られてきた`AppendEntriesCall`および`InstallSnapshotCast`を処理する.
pub struct FollowerIdle<IO: Io> {
    _phantom: PhantomData<IO>,
}
impl<IO: Io> FollowerIdle<IO> {
    pub fn new() -> Self {
        FollowerIdle {
            _phantom: PhantomData,
        }
    }

    #[allow(clippy::if_same_then_else)]
    pub fn handle_message(
        &mut self,
        common: &mut Common<IO>,
        message: Message,
    ) -> Result<NextState<IO>> {
        match message {
            Message::AppendEntriesCall(m) => track!(self.handle_entries(common, m)),
            Message::InstallSnapshotCast(m) => {
                if m.prefix.tail.index <= common.log_committed_tail().index {
                    // 既にコミット済みの地点のスナップショットは無視する
                    // (必要なら、ローカルノードで独自にスナップショットを取れば良い)
                    Ok(None)
                } else if common.is_snapshot_installing() {
                    // 別のスナップショットをインストール中
                    Ok(None)
                } else {
                    // 未コミット地点のスナップショットが送られてきた
                    // => リーダのログに、これ以前のエントリが残っていない可能性が
                    //    高いので、ローカルログにインストール必要がある
                    track!(common.install_snapshot(m.prefix))?;
                    let next = FollowerSnapshot::new();
                    Ok(Some(RoleState::Follower(Follower::Snapshot(next))))
                }
            }
            _ => Ok(None),
        }
    }

    fn handle_entries(
        &mut self,
        common: &mut Common<IO>,
        mut message: AppendEntriesCall,
    ) -> Result<NextState<IO>> {
        // `AppendEntriesCall`で受け取ったエントリ群が、ローカルログの末尾に追記可能になるように調整する

        let local_tail = common.log().tail();

        println!("< HANDLE ENTRIES! >");
        dbg!(common.local_node());        
        dbg!(common.log());
        dbg!(&message);
        println!("</ HANDLE ENTRIES! >");
        
        if message.suffix.tail().index < common.log().head().index {
            // リーダのログが、ローカルログに比べて大幅に短い (i.e., スナップショット地点以前)
            // => チャンネルに任意のメッセージ遅延を許している以上発生し得る
            common
                .rpc_callee(&message.header)
                .reply_append_entries(local_tail); // 処理はせずに、自分のログ終端を通知するに留める
            return Ok(None);
        }
        if message.suffix.head.index < common.log().head().index {
            // リーダのログが、ローカルのスナップショット地点以前のエントリを含んでいる
            // => その部分は、切り捨てる
            track!(message.suffix.skip_to(common.log().head().index))?;
        }

        if local_tail.index < message.suffix.head.index {
            // リーダのログが先に進み過ぎている
            // => 自分のログの末尾を伝えて、再送して貰う
            common
                .rpc_callee(&message.header)
                .reply_append_entries(local_tail);
            Ok(None)
        } else {
            // リーダのログとローカルのログに重複部分があり、追記が行える可能性が高い
            track!(self.handle_non_disjoint_entries(common, message))
        }
    }

    // overlapped_entriesとかに名前を変えた方が良い気がする
    // Heartbeatでもここまで来るか？？
    fn handle_non_disjoint_entries(
        &mut self,
        common: &mut Common<IO>,
        mut message: AppendEntriesCall,
    ) -> Result<NextState<IO>> {
        println!("<HANDLE_NON_DISJOINT_ENTRIES>");
        dbg!(common.local_node());
        dbg!(common.log());
        dbg!(&message.suffix);
        println!("</HANDLE_NON_DISJOINT_ENTRIES>");
        
        // リーダとローカルのログの共通部分を探索
        let (matched, lcp) = track!(self.longest_common_prefix(common, &message.suffix))?;

        dbg!(matched);
        dbg!(&lcp);
        
        if !matched {
            // 両者が分岐している
            // => ローカルログ(の未コミット領域)をロールバックして、同期位置まで戻る
            //
            // この場合分けが必要な理由は何か??
            // suffixで上書きしてしまえば良いのではないか??
            let new_log_tail = lcp;
            track!(common.handle_log_rollbacked(new_log_tail))?;
            common
                .rpc_callee(&message.header)
                .reply_append_entries(new_log_tail);
            Ok(None)
        } else {
            // 両者は包含関係にあるので、追記が可能
            track!(message.suffix.skip_to(lcp.index))?;
            let next = FollowerAppend::new(common, message);
            Ok(Some(RoleState::Follower(Follower::Append(next))))
        }
    }

    // Namingが非常に悪い。
    // このような関数名をつける場合に、引数にcommonが現れてはならない。
    //
    // `suffix`がleaderから来ているので信用するべき情報で、
    // commonに関しては疑うべきもの、という状況。
    //
    // ここで行っている計算は、
    // `suffix`を先頭から読んでいって、
    // common側のlocalな知識とどこまでagreeできるか。
    //
    // まだ不明瞭な点
    // 1. どこまで合意できるかを返す必要がなぜあるのか =>
    // leaderが知っておく必要があるのかもしれないが
    // 結局同じデータが再送されてくるなら無駄ではないか?
    // 
    // もちろんsuffix以前のところで不一致しているなら話は別であるが
    // その場合はここには来ないので
    fn longest_common_prefix(
        &self,
        common: &Common<IO>,
        suffix: &LogSuffix, // 関数名にprefix、引数にsuffixは酷すぎて読めない
    ) -> Result<(bool, LogPosition)> {
        // dbg!(&common.log());
        // dbg!(suffix);

        // heart beatが来た時には基本的にはそのまま返す……??
        for LogPosition { prev_term, index } in suffix.positions() {
            // prev_term と prev_index ではないことに注意。
            // indexはsuffixの開始位置で、prev_termは直前のエントリのterm
            
            // このprev_term --^  は本当にひとつ前のエントリのtermを表しているか?
            // どこで計算されている?
            // leaderのappender.rsでappendが呼ばれるたびに
            // common.log().tail()をpositionにしている。
            // append_tailが追加開始位置を表しているなら、
            // 一つ前を示しているということで良いのかなあ。

            // .get_recordの定義はやや奇妙なので注意
            let record = track!(
                common
                    .log()
                // logを作る時に+1しているので、suffixから見るとこのアクセスで直前を
                // みていることになる?
                    .get_record(index) 
                    .ok_or_else(|| ErrorKind::InconsistentState.error())
            )?;

            // println!("[idle.rs] prev_term = {:?}, record = {:?}", &prev_term, &record);
            
            let local_prev_term = record.head.prev_term;
            if prev_term != local_prev_term {
                // 両者のログが分岐
                let mut lcp = track!(common
                    .log()
                    .get_record(index - 1) // ここで-1にしているのは何故?? record_appendedで+1しているから?
                    .ok_or_else(|| ErrorKind::InconsistentState.error()))?
                .head;
                lcp.index = index - 1;
                return Ok((false, lcp));
            }

            // ここに来るのはいつ?
            // 前から順番に追加条件を検査していって、全てパスした時
            // 送られて来たsuffixが「新規追加(overlapなし)」の場合に
            // common.log().get_recordでerrorになりそうなのはどう説明する。
            if index == common.log().tail().index {
                // リーダのログが、ローカルログを包含
                return Ok((true, common.log().tail()));
            }
        }

        // ローカルログが、リーダのログを包含
        Ok((true, suffix.tail()))
    }
}
