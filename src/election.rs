//! リーダ選出関連の構成要素群.
use crate::node::NodeId;

/// Termを意味する自然数
///
/// Termによって、基本区間（選挙中期間またはリーダの連続在位期間）を表す。
///
/// 0-originで単調増加する。
/// 増加のタイミングについては、「新たな選挙を開始する」または「新しいリーダが統治開始」する２つがある。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Term(u64);
impl Term {
    /// 値が`number`となる`Term`インスタンスを生成する.
    pub fn new(number: u64) -> Self {
        Term(number)
    }

    /// このインスタンスの期間番号の値を返す.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}
impl From<u64> for Term {
    fn from(f: u64) -> Self {
        Term::new(f)
    }
}

/// あるノードの1つの投票内容を表す.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ballot {
    /// どの選挙での投票かを識別するための期間番号.
    pub term: Term,

    /// 投票先.
    pub voted_for: NodeId,
}

impl Ballot {
    pub fn to_str(&self) -> String {
        format!(
            "ballot(term: {}, for: {})",
            self.term.as_u64(),
            self.voted_for.as_str()
        )
    }
}

/// あるノードの選挙におけるノードの役割.
/// FIX: termごとに考えるものでなくて良いか?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 他の候補者(or リード)に投票済み.
    Follower,

    /// 選挙の立候補者.
    Candidate,

    /// 過半数以上の投票を集めて選出されたリーダ.
    Leader,
}
