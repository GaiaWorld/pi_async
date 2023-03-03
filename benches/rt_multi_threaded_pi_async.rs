#![feature(test)]

extern crate test;

use futures::channel::oneshot;
use futures::Future;
use pi_async::prelude::{MultiTaskRuntime, MultiTaskRuntimeBuilder, StealableTaskPool};
use pi_async::rt::{AsyncRuntime, AsyncRuntimeExt, TaskId};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::SyncSender;
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll};
use test::Bencher;

fn runtime() -> MultiTaskRuntime<()> {
    let pool = StealableTaskPool::with(8, 8);
    let builer = MultiTaskRuntimeBuilder::new(pool)
        .set_timer_interval(1)
        .init_worker_size(8)
        .set_worker_limit(8, 8);
    builer.build()
}

#[bench]
fn spawn_many(b: &mut Bencher) {
    let rt = runtime();
    const NUM_SPAWN: usize = 10_000;
    let (tx, rx) = mpsc::sync_channel(1000);
    let rem = Arc::new(AtomicUsize::new(0));

    b.iter(|| {
        let tx = tx.clone();
        rem.store(NUM_SPAWN, Relaxed);
        let rt_copy = rt.clone();
        let rem = rem.clone();

        rt.block_on(async move {
            for _ in 0..NUM_SPAWN {
                let rem = rem.clone();
                let tx = tx.clone();
                rt_copy
                    .spawn(rt_copy.alloc(), async move {
                        // let tx_copy: SyncSender<()> = tx.clone();
                        if 1 == rem.fetch_sub(1, Relaxed) {
                            tx.send(()).unwrap();
                        }
                    })
                    .unwrap();
            }
        })
        .unwrap();
        let _ = rx.recv().unwrap();
    });
}

struct TestFuture1(TaskId, MultiTaskRuntime<()>);

unsafe impl Send for TestFuture1 {}
unsafe impl Sync for TestFuture1 {}

impl Future for TestFuture1 {
    type Output = String;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.1.pending(&self.0, cx.waker().clone())
    }
}

impl TestFuture1 {
    pub fn new(rt: MultiTaskRuntime<()>, uid: TaskId) -> Self {
        TestFuture1(uid, rt)
    }
}

// #[bench]
// fn yield_many(b: &mut Bencher) {
//     let rt = runtime();
//     const NUM_YIELD: usize = 1_000;
//     const TASKS: usize = 200;

//     let (tx, rx) = mpsc::sync_channel(TASKS);
//     let rt_copy = rt.clone();
//     b.iter(move || {
//         for _ in 0..TASKS {
//             let tx = tx.clone();
//             let task_id = rt_copy.alloc();
//             let task_id_copy = task_id.clone();
//             let task_id_copy2 = task_id.clone();

//             let rt_copy2 = rt_copy.clone();
//             rt_copy
//                 .spawn(task_id, async move {
//                     for _ in 0..NUM_YIELD {
//                         // TODO
//                         let future_test = TestFuture1::new(rt_copy2.clone(), task_id_copy.clone());
//                         future_test.await;

//                         // task::yield_now().await;
//                     }

//                     tx.send(()).unwrap();
//                 })
//                 .unwrap();
//             // rt_copy.wakeup(&task_id_copy2);
//         }

//         for _ in 0..TASKS {
//             let _ = rx.recv().unwrap();
//         }
//     });
// }

#[bench]
fn ping_pong(b: &mut Bencher) {
    let rt = runtime();

    const NUM_PINGS: usize = 1_000;

    let (done_tx, done_rx) = mpsc::sync_channel(1000);
    let rem = Arc::new(AtomicUsize::new(0));

    b.iter(|| {
        let rt_copy = rt.clone();
        let done_tx: SyncSender<()> = done_tx.clone();
        let rem = rem.clone();
        rem.store(NUM_PINGS, Relaxed);

        rt.block_on(async move {
            let rt_copy2 = rt_copy.clone();
            rt_copy
                .spawn(rt_copy.alloc(), async move {
                    for _ in 0..NUM_PINGS {
                        let rem = rem.clone();
                        let done_tx = done_tx.clone();
                        let rt_copy3 = rt_copy2.clone();
                        rt_copy2
                            .spawn(rt_copy2.alloc(), async move {
                                let (tx1, rx1) = oneshot::channel();
                                let (tx2, rx2) = oneshot::channel();

                                rt_copy3
                                    .spawn(rt_copy3.alloc(), async move {
                                        rx1.await.unwrap();
                                        tx2.send(()).unwrap();
                                    })
                                    .unwrap();

                                tx1.send(()).unwrap();
                                rx2.await.unwrap();

                                if 1 == rem.fetch_sub(1, Relaxed) {
                                    done_tx.send(()).unwrap();
                                }
                            })
                            .unwrap();
                    }
                })
                .unwrap();
        })
        .unwrap();
        done_rx.recv().unwrap();
    });
}

#[bench]
fn chained_spawn(b: &mut Bencher) {
    let rt = runtime();
    const ITER: usize = 1_000;

    fn iter(rt: MultiTaskRuntime, done_tx: mpsc::SyncSender<()>, n: usize) {
        if n == 0 {
            done_tx.send(()).unwrap();
        } else {
            let rt_copy = rt.clone();
            rt.spawn(rt.alloc(), async move {
                iter(rt_copy.clone(), done_tx, n - 1);
            })
            .unwrap();
        }
    }

    let (done_tx, done_rx) = mpsc::sync_channel(1000);

    b.iter(move || {
        let rt_copy = rt.clone();
        let rt_copy2 = rt.clone();
        let rt_copy3 = rt.clone();
        let done_tx = done_tx.clone();

        rt_copy
            .block_on(async move {
                rt_copy2
                    .spawn(rt_copy2.alloc(), async move {
                        iter(rt_copy3.clone(), done_tx, ITER);
                    })
                    .unwrap();
            })
            .unwrap();
        done_rx.recv().unwrap();
    });
}
