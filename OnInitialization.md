# Initializationの問題点

現コードは、初期化処理についてユーザが気にするべき点について何も明らかにされていない。

まず初期化処理についてユーザが実装しなければならない関数として次のようなものが挙げられる:
* https://github.com/yuezato/raftlog2/blob/main/src/io.rs#L60
* https://github.com/yuezato/raftlog2/blob/main/src/io.rs#L84

一般的に、初期化処理は「完全に新規の計算を開始する」場合と「resumeを行う」場合とで
ドキュメントレベルなどで十分注意が払われなければならない。
そのような場合分けが必要ではない、またはライブラリ内部（ユーザが見えない部分）で
手厚く処理される場合はドキュメントは不要であるが、raflogはそのようになっていない。
例えば、上記2つ目の関数の実装として

```rust
let log = LogSuffix {
  entries: Vec::new(),
  head: LogPosition {
  index: LogIndex::new(0),
  prev_term: Term::new(0xff), // 空に対してのprev_termはどうする???
};
```

のようなTermを決めかねる（決定する術が一切ない）ため、`0xff`のような値を渡すと

```
thread 'main' panicked at 'attempt to subtract with overflow', src/log/mod.rs:81:18
stack backtrace:
   0: rust_begin_unwind
             at /rustc/cb75ad5db02783e8b0222fee363c5f63f7e2cf5b/library/std/src/panicking.rs:493:5
   1: core::panicking::panic_fmt
             at /rustc/cb75ad5db02783e8b0222fee363c5f63f7e2cf5b/library/core/src/panicking.rs:92:14
   2: core::panicking::panic
             at /rustc/cb75ad5db02783e8b0222fee363c5f63f7e2cf5b/library/core/src/panicking.rs:50:5
   3: <raftlog::log::LogIndex as core::ops::arith::Sub<usize>>::sub
             at ./src/log/mod.rs:81:18
   4: raftlog::node_state::follower::idle::FollowerIdle<IO>::longest_common_prefix
             at ./src/node_state/follower/idle.rs:139:33
   5: raftlog::node_state::follower::idle::FollowerIdle<IO>::handle_non_disjoint_entries
             at ./src/node_state/follower/idle.rs:92:37
   6: raftlog::node_state::follower::idle::FollowerIdle<IO>::handle_entries
             at ./src/node_state/follower/idle.rs:83:20
   7: raftlog::node_state::follower::idle::FollowerIdle<IO>::handle_message
             at ./src/node_state/follower/idle.rs:30:53
   8: raftlog::node_state::follower::Follower<IO>::handle_message
             at ./src/node_state/follower/mod.rs:58:49
   9: raftlog::node_state::NodeState<IO>::handle_message
             at ./src/node_state/mod.rs:74:28
  10: <raftlog::node_state::NodeState<IO> as futures::stream::Stream>::poll
             at ./src/node_state/mod.rs:169:44
  11: <raftlog::replicated_log::ReplicatedLog<IO> as futures::stream::Stream>::poll
             at ./src/replicated_log.rs:299:16
```

のようにしてシステムが起動しない。

新規計算開始時にどのように初期値を設定するか（及び何故そうするか）が
完全に説明されていなければならない。

# なぜ問題が生じるか
先頭Termを「非ゼロ」にすると、この部分でアンダーフローが生じてしまう。
https://github.com/yuezato/raftlog2/blob/f217ab3d3f28f1426c2212a937be5527753aafb7/src/node_state/follower/idle.rs#L122-L125

# どうすれば解決できるか?
「現状では」ロードすることのできるデータがない場合は、当該箇所に「ゼロ値」採用することになる。
次に問題になるのは、なぜ「ゼロ値」なら問題がないかということである。
