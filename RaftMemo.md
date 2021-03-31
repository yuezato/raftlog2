# バグを疑っている箇所

* `cluster.rs/median`及びそれを使っている箇所
* [Frugalosのこのコメント直せる?](https://github.com/frugalos/frugalos/blob/84898a76ee6e8f0f68159769f1ea964dfa5c04bd/frugalos_mds/src/node/node.rs#L574)
* [ReadConsistency](https://github.com/frugalos/libfrugalos/blob/master/src/consistency.rs)はデフォルトだとConsistentで、
これは[自分がリーダーだと分かれば](https://github.com/frugalos/frugalos/blob/4decaea4131bbf73e2229b9b52d880b20a289324/frugalos_mds/src/node/node.rs#L411)ヨシとする設定ではある。
    * しかしstale leaderの場合には、実際にはある（running subclusterでput済み）ものをないと答えたり、既に削除されたものをある（running subclusterで削除済み）と答える可能性がある。
    * これを解決する良い方法は何かあるか? 例えば新しいleaderが決まる度にその情報を送るような方法もあるが、たまたまそのパケットだけロストしてしまった場合には、というような話がある。
    * ハートビートをタイムアウトを使って定常的にやっている場合は、stale leaderかどうかに気づけるが、究極的に間を縫われるとキツイという話。

# raftlog2/main.rsでやること

* 様々な状況を合理的に構成できるように、transition systemに対する操作をできるように、指定したコマンドを順次解釈・実行できるようにする
* 最初はconfigurationの部分だけやってみる; configurationはデカイので
    * まずどこからやってみるべきか?

# Raftの解説

Core raftとメンバ構成変更に分けて説明する。

## その前に先に念頭に入れておくべきこと
CAP schemaでいうところのC(onsistency)とP(artial torelance)重視なので
A(vailability)が欠けてしまう。

システムと切り離された外部からは応答不能状態、すなわち
システムが「全く動いていない」（実際は内部で忙しくしていても）
ように見えることがある。

## Core Raft
計算は2つの要素から構成される:
1. Leaderを選出すること
2. LeaderからReplicaにデータを送りつけること。

MultiPaxosと似ていないというのは難しいので、
仕方なく似ていると言うことにするが、
ReplicaからLeaderにデータを送りつけることがない決定的な違いある。

したがって、過半数票を集まれば乗っ取れるMultiPaxosとは違い、
Leaderは「過半数表を得る」ことに加えて「資質」を満たしている必要がある。

資質とは、最後のtermに本質的に参加していた過半数いるはずのメンバーであること。
過半数のうちで最もデータを蓄えているものである必要はない。
従ってある種のRoll Backが起こっている可能性があるが、
これはcommitしていないからと思うことにすれば問題ない。

## メンバ構成変更
ただし、資質を満たす上ではメンバ構成変更について注意する必要がある。
裏で勝手にメンバが増えてしまった場合には、資質が絶対に満たされなくなるからである。


# RaftLogは何を実装しているか
* Core raft
* メンバ構成変更

オリジナルの論文にない工夫について。

## Implementation
```
src
├── cluster.rs (Raftクラスタに関するコード群)
├── election.rs 選挙のための基本構造 (Term, Ballot, Role)
├── error.rs (このライブラリのエラー表現を束ねるもの)
├── io.rs (I/O処理を抽象化したtrait; I/O処理はストレージI/OとネットワークI/Oの二種類)
├── lib.rs (このライブラリを総括するモジュール)
├── log
│   ├── history.rs（ログではないhistoryという謎の構造を定義している どこで使われている?）
│   └── mod.rs（ログを定義しているが複雑すぎる）
├── message.rs（メッセージパッシング用の構造体など）
├── metrics.rs（Prometheusなどで使うためのもので計算には無関係）
├── node.rs（Raft clusterの基本構成単位であるノードを表す構造体等）
├── node_state（各nodeでの計算を行うためのもの。基本的に複雑すぎる）
│   ├── candidate.rs (candidate stateの抽象)
│   ├── common (candidate, follower, leaderのsuper class)
│   │   ├── mod.rs
│   │   └── rpc_builder.rs (RPCの抽象化)
│   ├── follower (follower stateの抽象)
│   │   ├── append.rs
│   │   ├── idle.rs
│   │   ├── init.rs
│   │   ├── mod.rs
│   │   └── snapshot.rs
│   ├── leader
│   │   ├── appender.rs
│   │   ├── follower.rs
│   │   └── mod.rs
│   ├── loader.rs
│   └── mod.rs (LogHistoryはこいつが持っている)
├── replicated_log.rs（命名がかなり悪い。これはraft clusterそのものまたはreplicated state machineのこと）
└── test_util.rs
```
