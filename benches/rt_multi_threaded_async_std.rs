#![feature(test)]

extern crate test;

use async_std::task;
use futures::channel::oneshot;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{mpsc, Arc};
use test::Bencher;

#[bench]
fn spawn_many(b: &mut Bencher) {
    const NUM_SPAWN: usize = 10_000;

    let (tx, rx) = mpsc::sync_channel(1000);
    let rem = Arc::new(AtomicUsize::new(0));

    b.iter(|| {
        rem.store(NUM_SPAWN, Relaxed);

        task::block_on(async {
            for _ in 0..NUM_SPAWN {
                let tx = tx.clone();
                let rem = rem.clone();

                task::spawn(async move {
                    if 1 == rem.fetch_sub(1, Relaxed) {
                        tx.send(()).unwrap();
                    }
                });
            }

            let _ = rx.recv().unwrap();
        });
    });
}

#[bench]
fn yield_many(b: &mut Bencher) {
    const NUM_YIELD: usize = 1_000;
    const TASKS: usize = 200;

    let (tx, rx) = mpsc::sync_channel(TASKS);

    b.iter(move || {
        for _ in 0..TASKS {
            let tx = tx.clone();

            task::spawn(async move {
                for _ in 0..NUM_YIELD {
                    task::yield_now().await;
                }

                tx.send(()).unwrap();
            });
        }

        for _ in 0..TASKS {
            let _ = rx.recv().unwrap();
        }
    });
}

#[bench]
fn ping_pong(b: &mut Bencher) {
    const NUM_PINGS: usize = 1_000;

    let (done_tx, done_rx) = mpsc::sync_channel(1000);
    let rem = Arc::new(AtomicUsize::new(0));

    b.iter(|| {
        let done_tx = done_tx.clone();
        let rem = rem.clone();
        rem.store(NUM_PINGS, Relaxed);

        task::block_on(async {
            task::spawn(async move {
                for _ in 0..NUM_PINGS {
                    let rem = rem.clone();
                    let done_tx = done_tx.clone();

                    task::spawn(async move {
                        let (tx1, rx1) = oneshot::channel();
                        let (tx2, rx2) = oneshot::channel();

                        task::spawn(async move {
                            rx1.await.unwrap();
                            tx2.send(()).unwrap();
                        });

                        tx1.send(()).unwrap();
                        rx2.await.unwrap();

                        if 1 == rem.fetch_sub(1, Relaxed) {
                            done_tx.send(()).unwrap();
                        }
                    });
                }
            });

            done_rx.recv().unwrap();
        });
    });
}

#[bench]
fn chained_spawn(b: &mut Bencher) {
    const ITER: usize = 1_000;

    fn iter(done_tx: mpsc::SyncSender<()>, n: usize) {
        if n == 0 {
            done_tx.send(()).unwrap();
        } else {
            task::spawn(async move {
                iter(done_tx, n - 1);
            });
        }
    }

    let (done_tx, done_rx) = mpsc::sync_channel(1000);

    b.iter(move || {
        let done_tx = done_tx.clone();

        task::block_on(async {
            task::spawn(async move {
                iter(done_tx, ITER);
            });

            done_rx.recv().unwrap();
        });
    });
}
