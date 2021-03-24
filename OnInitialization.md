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

より正確に言うならば、
```
commong.log = records: [
        HistoryRecord {
            head: LogPosition {             |
                prev_term: Term(            |
                    0,                      |
                ),                          | <-----+
                index: LogIndex(            |       | 
                    0,                      |       | 
                ),                          |       | 
            },                              |       | 
            config: ClusterConfig {                 |
                new: {                              |
                    NodeId(                         |
                        "nodeA",                    |
                    ),                              |
                    NodeId(                         |
                        "nodeB",                    |
                    ),                              |
                },                                  |
                old: {},                            |
                state: Stable,                      |
            },                                      |
        },                                          |
    ],                                              |
}                                                   |
                                                    |
suffix = LogSuffix {                                |
    entries: [                                      |
        Noop {                                      |
            term: Term(                             |
                2,                                  |
            ),                                      |
        },                                          |
    ],                                              |
    head: LogPosition {      |                      |
        prev_term: Term(     |                      |
            255,             |                      |
        ),                   |                      |
        index: LogIndex(     |<---------------------+
            0,               |
        ),                   |
    },                       |
}
```

によって上の問題が生じることが分かる。 `common.log` 側ではなぜか先頭に `prev_term = 0` のエントリが存在しているが、
`suffix`側では先頭に `prev_term = 255` のエントリが存在している。
この食い違いが問題を産んでいる。

`suffix`側はinitializeで採用した値が使われているが、一方で`common.log`側では謎のエントリが設定されていることになる。
次はこれがどこから現れるかを調べる必要がある。

これは
1. https://github.com/yuezato/raftlog2/blob/main/src/node_state/mod.rs#L37
2. https://github.com/yuezato/raftlog2/blob/main/src/node_state/common/mod.rs#L48
3. https://github.com/yuezato/raftlog2/blob/main/src/log/history.rs#L32
という処理の流れによって生じることが（追跡すると）分かる。

なぜか空リストではなくサイズ1のリストを作っているが、このようにした理由はどこにも記載されていない。

# どのようにすれば良いか?
https://github.com/yuezato/raftlog2/blob/740555c5842be639590ecc31d647c24bdc33832b/src/log/history.rs#L26-L34

`records`の初期値を「非常に好意的に」解釈するならば、（古いプログラミングの本で言うところの）番兵ということになるが、
このように考えたとしてもtail値が既に存在するフィールドを指していることは理解できない。

というのは、（これも太田さんがどうしたかったのかわからないが）tail値たちはexclusive rangeを取るようで、例えば
`appended_tail`というのは`[0, appended_tail)`のrangeをもってその意味を成すということになる。
そのように判断して良い理由はこの箇所にある
https://github.com/yuezato/raftlog2/blob/740555c5842be639590ecc31d647c24bdc33832b/src/log/history.rs#L82

こう考えると、初期化では`appended_tail = LogPosiiton::default() + 1`のようにしなくてはならない。
ならないのだが、そうなっていない。

まず気にするべき点は、読み込むべきデータがなにもない場合の(Termではなく)Indexはどうするかという点である。
https://github.com/frugalos/frugalos/blob/bdb58d47ef50e5f7038bdce4b6cd31736260d6e8/frugalos_raft/src/storage/mod.rs#L112-L164

この辺のコードを読むと、Indexは0開始で問題がなさそうということになる。

これが仮定出来ると必然的にTermは0に *するしかない* ため、結果的にこれを使うことと同じことになる。
https://github.com/yuezato/raftlog2/blob/05ca296b91954b91f1ebcdd4d9c55e0b2177ab13/src/log/mod.rs#L161-L168

「するしかない」というのは論理的な帰結ではなく、こうしないとpanicして全く使い物にならないからということであって、
状況としては非常に悪い。
