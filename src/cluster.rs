//! クラスタ構成関連.
//!
//! なお、クラスタ構成の動的変更に関する詳細は、
//! [Raftの論文](https://raft.github.io/raft.pdf)の「6 Cluster membership changes」を参照のこと.
use std::cmp;
use std::collections::BTreeSet;

use crate::node::NodeId;

/// クラスタに属するメンバ群.
pub type ClusterMembers = BTreeSet<NodeId>;

/// クラスタの状態.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterState {
    /// 構成変更中ではなく安定している状態.
    Stable,

    /// 構成変更の第一ステージとして
    /// 新構成のうち追加することになるメンバへログを送っている状態
    ///
    /// この状態では旧構成のメンバのみが実行権を持つ（majorityのカウントに使われる）
    CatchUp,

    /// 構成変更の第二ステージで
    /// 新旧メンバ群の両方に合意が必要な状態.
    /// ログの同期が終わっていて（これは過半数が承知しているということ?）で
    /// 構成変更が浸透するのを待っている状況である。
    Joint,
}
impl ClusterState {
    /// 安定状態かどうかを判定する.
    pub fn is_stable(self) -> bool {
        self == ClusterState::Stable
    }

    /// 新旧混合状態かどうかを判定する.
    pub fn is_joint(self) -> bool {
        self == ClusterState::Joint
    }
}

/// クラスタ構成.
///
/// クラスタに属するメンバの集合に加えて、
/// 動的構成変更用の状態を管理する.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    new: ClusterMembers,
    old: ClusterMembers,
    state: ClusterState,
}
impl ClusterConfig {
    /// 現在のクラスタ状態を返す.
    pub fn state(&self) -> ClusterState {
        self.state
    }

    /// 構成変更後のメンバ集合が返される.
    ///
    /// 安定状態では、現在のメンバ群が返される
    /// (i.e, `members`メソッドが返すメンバ群と等しい).
    pub fn new_members(&self) -> &ClusterMembers {
        &self.new
    }

    /// 構成変更前のメンバ集合が返される.
    ///
    /// 安定状態では、空集合が返される.
    pub fn old_members(&self) -> &ClusterMembers {
        &self.old
    }

    /// "プライマリメンバ"の全体集合Pを返す。
    ///
    /// プライマリメンバ: 合意に有効なメンバ
    /// サービスの整合性を担保するには、
    /// 単なる過半数以上の合意ではなく、「Pから過半数以上の合意」を得なくてはならない。
    ///
    /// メンバとプライマリメンバの区別は、構成変更過渡期で必要になる。
    /// 安定状態では、メンバはプライマリメンバとして見て良い。
    /// 構成変更時には、旧構成に属するメンバのみがプライマリメンバとなる。
    pub fn primary_members(&self) -> &ClusterMembers {
        match self.state {
            ClusterState::Stable => &self.new,
            ClusterState::CatchUp => &self.old,
            ClusterState::Joint => &self.old,
        }
    }

    /// クラスタに属するメンバ群を返す.
    ///
    /// 構成変更中の場合には、新旧両方のメンバの和集合が返される.
    pub fn members(&self) -> impl Iterator<Item = &NodeId> {
        self.new.union(&self.old)
    }

    /// このクラスタ構成に含まれるノードかどうかを判定する.
    pub fn is_known_node(&self, node: &NodeId) -> bool {
        self.new.contains(node) || self.old.contains(node)
    }

    /// 安定状態の`ClusterConfig`インスタンスを生成する.
    pub fn new(members: ClusterMembers) -> Self {
        ClusterConfig {
            new: members,
            old: ClusterMembers::default(),
            state: ClusterState::Stable,
        }
    }

    /// `state`を状態とする`ClusterConfig`インスタンスを生成する.
    pub fn with_state(
        new_members: ClusterMembers,
        old_members: ClusterMembers,
        state: ClusterState,
    ) -> Self {
        ClusterConfig {
            new: new_members,
            old: old_members,
            state,
        }
    }

    /// `new`を移行先状況とするような
    /// `CatchUp`状態の`ClusterConfig`インスタンスを返す.
    pub(crate) fn start_config_change(&self, new: ClusterMembers) -> Self {
        ClusterConfig {
            new,
            old: self.primary_members().clone(),
            state: ClusterState::CatchUp,
        }
    }

    /// 次の状態に遷移する.
    ///
    /// # 状態遷移表
    ///
    ///                         v------|
    /// CatchUp --> Joint --> Stable --|
    pub(crate) fn to_next_state(&self) -> Self {
        match self.state {
            ClusterState::Stable => self.clone(),
            ClusterState::CatchUp => {
                let mut next = self.clone();
                next.state = ClusterState::Joint;
                next
            }
            ClusterState::Joint => {
                let mut next = self.clone();
                next.old = ClusterMembers::new(); // Stableではoldは空集合
                next.state = ClusterState::Stable;
                next
            }
        }
    }

    /// クラスタの合意値.
    //
    /// `f`は関数で、メンバごとの承認値を表す。
    /// f(m) = xは次の意味でm中で最大である:
    /// mの中でy < xであればyも承認済み。
    ///
    /// クラスタ合意値は「メンバの過半数によって承認されている最大の値」
    pub(crate) fn consensus_value<F, T>(&self, f: F) -> T
    where
        F: Fn(&NodeId) -> T,
        T: Ord + Copy + Default,
    {
        match self.state {
            ClusterState::Stable => median(&self.new, &f),
            ClusterState::CatchUp => median(&self.old, &f),
            ClusterState::Joint => {
                // joint consensus
                cmp::min(median(&self.new, &f), median(&self.old, &f))

                // FIX
                // median(self.new + self.old, f)でダメな理由は何？
                // 一気に大量のnewが加わってそこではcommitしているかもしれないが
                // まだrunningしているold側ではcommitしていないような状況では
                // 最終的な移行失敗時に備えないといけない
                // Joint中はその構成に映ることが確定しているわけではないので
                // Joint中にnewを無視すると一方でどうなる??
            }
        }
    }

    /// Stable, Jointについては`consensus_value`メソッドと同様.
    ///
    /// Catchup(構成変更中)では、新旧メンバ群の両方から、
    /// 過半数の承認を要求するところが異なる.
    pub(crate) fn full_consensus_value<F, T>(&self, f: F) -> T
    where
        F: Fn(&NodeId) -> T,
        T: Ord + Copy + Default,
    {
        if self.state.is_stable() {
            median(&self.new, &f)
        } else {
            // joint & catchup consensus
            cmp::min(median(&self.new, &f), median(&self.old, &f))
        }
    }
}

/// 1 => 1
/// 2 => 2, 3 => 2,
/// 4 => 3, 5 => 3,
/// 2n => n+1, 2n+1 => n+1
fn majority(n: usize) -> usize {
    debug_assert!(n >= 1);
    1 + (n/2)
}

/// This returns the `maximum` one of the agreed values (= the values supported by majority).
/// A value `v` is supported by majority
/// if #{ m \in ClusterMembers : v <= f(m) } >= majority(#ClusterMembers)
/// where majority(2n) = n+1, majority(2n+1) = n+1.
fn max_of_agreed_value<T>(mut v: Vec<T>) -> T
where
    T: Ord + Copy
{
    debug_assert!(!v.is_empty());
    let majority = majority(v.len()); // majority >= 1 always hold

    v.sort_unstable(); 
    // for strictly ascending sequences, values[0] < values[1] < ...
    // values[0] is agreed and supported by all members
    // values[1] is agreed and supported by #members-1 members
    // values[2] is agreed and supported by #members-2 members
    // ...
    // values[#members - majority] is agreed and supported by majority members
    // values[#members - majority + 1] is `not` agreed and supported by majority-1 members.
    // ...
    // values[#members-1] is supported by only one member.
    //
    // A similar argument holds for weakly ascending sequence
    // i.e., values[0] <= values[1] <= ...
    v[v.len()-majority]
}

fn median<F, T>(members: &ClusterMembers, f: F) -> T
where
    F: Fn(&NodeId) -> T,
    T: Ord + Copy
{
    if members.is_empty() {
        unreachable!("FIX");
    }
    let mapped = members.iter().map(|n| f(n)).collect::<Vec<_>>();
    max_of_agreed_value(mapped)
}


#[cfg(test)]
mod test {
    use super::*;
    extern crate itertools;
    use itertools::Itertools;

    /// Old implementation, which uses Vec::reverse
    /// https://github.com/frugalos/raftlog/blob/087d8019b42a05dbc5b463db076041d45ad2db7f/src/cluster.rs#L196
    fn orig_median<T>(mut values: Vec<T>) -> T
    where
        T: Ord + Copy,
    {
        values.sort(); // v[0] < v[1] < ...
        values.reverse(); // v[0] > v[1] > ...
        if values.is_empty() {
            unreachable!("FIX");
        } else {
            values[values.len() / 2]
        }
    }
    
    #[test]
    fn majority_works() {
        assert_eq!(majority(1), 1);
        assert_eq!(majority(2), 2);
        assert_eq!(majority(3), 2);
        assert_eq!(majority(4), 3);
        assert_eq!(majority(5), 3);
        assert_eq!(majority(6), 4);
        assert_eq!(majority(7), 4);
        assert_eq!(majority(8), 5);
        assert_eq!(majority(9), 5);
    }

    fn naively_compute_agreed_values(mut v: Vec<usize>) -> usize {
        use std::collections::HashMap;

        let mut map: HashMap<usize, usize> = HashMap::new();
        
        for e in &v {
            let c = v.iter().filter(|&x| *e <= *x).count();
            map.insert(*e, c);
        }

        let majority = majority(v.len());
        let filt: Vec<usize> = v.iter().filter(|e| map[&e] >= majority).cloned().collect();
        *filt.iter().max().unwrap()
    }
    
    #[test]
    fn max_of_agreed_value_works() {
        for i in 1..=10 {
            for v in (0..i).combinations_with_replacement(i) {
                let x1 = max_of_agreed_value(v.clone());
                let x2 = naively_compute_agreed_values(v.clone());
                let x3 = orig_median(v.clone());
                
                assert_eq!(x1, x2);
                assert_eq!(x2, x3);
            }
        }
    }
}
