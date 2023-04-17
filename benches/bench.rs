#![feature(test)]
extern crate test;

use std::thread;
use test::Bencher;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded, unbounded};
use tokio::{runtime, spawn};

use pi_async::rt::AsyncRuntime;
use pi_async::rt::serial::AsyncRuntime as SerialAsyncRuntime;
use pi_async::rt::serial_local_thread::LocalTaskRunner;
use pi_async::rt::single_thread::SingleTaskRunner;
use pi_async::rt::multi_thread::{ComputationalTaskPool, MultiTaskRuntimeBuilder};
use pi_async::lock::mutex_lock::Mutex;

#[derive(Clone)]
struct AtomicCounter(Arc<Sender<()>>);
impl Drop for AtomicCounter {
    fn drop(&mut self) {
        self.0.send(()); //通知执行完成
    }
}

#[bench]
fn bench_spawn_by_local(b: &mut Bencher) {
    let runner = LocalTaskRunner::new();
    let rt = runner.get_runtime();

    let (sender, receiver) = unbounded();
    let counter = AtomicCounter(Arc::new(sender));

    b.iter(|| {
        for _ in 0..100000 {
            let counter_copy = counter.clone();
            rt.spawn(async move {
                         counter_copy;
                     });
            runner.run_once();
        }

        let _ = receiver.recv().unwrap();
    });
}

#[bench]
fn bench_spawn_by_single(b: &mut Bencher) {
    let runner = SingleTaskRunner::default();
    let rt = runner.startup().unwrap();

    thread::spawn(move || {
        loop {
            if let Err(e) = runner.run() {
                println!("!!!!!!run failed, reason: {:?}", e);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    let (sender, receiver) = unbounded();
    let counter = AtomicCounter(Arc::new(sender));

    b.iter(|| {
        for _ in 0..100000 {
            let counter_copy = counter.clone();
            rt.spawn(rt.alloc(),
                     async move {
                         counter_copy;
                     });
        }

        let _ = receiver.recv().unwrap();
    });
}

#[bench]
fn bench_spawn_by_multi(b: &mut Bencher) {
    let rt = MultiTaskRuntimeBuilder::default()
        .init_worker_size(4)
        .set_worker_limit(4, 4)
        .set_timeout(1)
        .build();
    let (sender, receiver) = unbounded();
    let counter = AtomicCounter(Arc::new(sender));

    b.iter(|| {
        for _ in 0..100000 {
            let counter_copy = counter.clone();
            rt.spawn(rt.alloc(),
                     async move {
                         counter_copy;
                     });
        }

        let _ = receiver.recv().unwrap();
    });
}

#[bench]
fn bench_spawn_by_single_for_tokio(b: &mut Bencher) {
    let rt = runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let (sender, receiver) = unbounded();
    let counter = AtomicCounter(Arc::new(sender));

    b.iter(|| {
        let counter_copy = counter.clone();
        rt.block_on(async move {
            for _ in 0..100000 {
                let counter_clone = counter_copy.clone();
                spawn(async move {
                    counter_clone;
                });
            }
        });

        let _ = receiver.recv().unwrap();
    });
}

#[bench]
fn bench_spawn_by_multi_for_tokio(b: &mut Bencher) {
    let rt = runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_time()
        .build()
        .unwrap();
    let (sender, receiver) = unbounded();
    let counter = AtomicCounter(Arc::new(sender));

    b.iter(|| {
        for _ in 0..100000 {
            let counter_copy = counter.clone();
            rt.spawn(async move {
                         counter_copy;
                     });
        }

        let _ = receiver.recv().unwrap();
    });
}