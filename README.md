基于Future(MVP)，用于为外部提供基础的通用异步运行时和工具

# 主要特征

- 任务池: 可定制的任务池
- 任务ID: 外部使用任务ID可以很方便的唤醒和挂起
- 抽象接口: 可以自由实现自己的运行时
- 运行时推动：单线程运行时可以用自己的方式推动运行

# Examples

本地异步运行时:
```
use pi_async::rt::serial_local_thread::{LocalTaskRunner, LocalTaskRuntime};
use pi_async::rt::AsyncRuntimeExt;
let rt = LocalTaskRunner::<()>::new().into_local();
rt.block_on(async {});
```
单线程异步运行时使用:
```
use pi_async::prelude::{SingleTaskPool, SingleTaskRunner};
let pool = SingleTaskPool::default();
let rt = SingleTaskRunner::<(), SingleTaskPool<()>>::new(pool).into_local();
let _ = rt.block_on(async {});
```
多线程异步运行时使用:
```
use pi_async::prelude::{MultiTaskRuntime, MultiTaskRuntimeBuilder, StealableTaskPool};
use pi_async::rt::AsyncRuntimeExt;
let pool = StealableTaskPool::with(4, 4);
let builer = MultiTaskRuntimeBuilder::new(pool)
    .set_timer_interval(1)
    .init_worker_size(4)
    .set_worker_limit(4, 4);
let rt = builer.build();
let _ = rt.spawn(rt.alloc(), async move {});
```

# 贡献指南

# License

This project is licensed under the [MIT license].

[MIT license]: LICENSE

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in pi_async by you, shall be licensed as MIT, without any additional
terms or conditions.