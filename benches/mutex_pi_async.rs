#![feature(test)]

extern crate test;

use std::sync::Arc;

use pi_async::{
    prelude::{Mutex, SingleTaskPool, SingleTaskRunner, SingleTaskRuntime},
    rt::{AsyncRuntime, AsyncRuntimeExt, AsyncValueNonBlocking},
};
use test::Bencher;

fn runtime() -> SingleTaskRuntime {
    let pool = SingleTaskPool::default();
    SingleTaskRunner::<(), SingleTaskPool<()>>::new(pool).into_local()
}

#[bench]
fn create(b: &mut Bencher) {
    b.iter(|| Mutex::new(()));
}

#[bench]
fn contention(b: &mut Bencher) {
    let rt = runtime();
    b.iter(|| {
        let rt_copy = rt.clone();
        rt.block_on(run(rt_copy, 10, 1000)).unwrap()
    });
}

#[bench]
fn no_contention(b: &mut Bencher) {
    let rt = runtime();
    b.iter(|| {
        let rt_copy = rt.clone();
        rt.block_on(run(rt_copy, 1, 10000)).unwrap()
    });
}

async fn run(rt: SingleTaskRuntime, task: usize, iter: usize) {
    let m = Arc::new(Mutex::new(()));
    let mut tasks = Vec::new();

    for _ in 0..task {
        let value = AsyncValueNonBlocking::new();
        let value_copy = value.clone();
        let m = m.clone();
        rt.spawn(rt.alloc(), async move {
            for _ in 0..iter {
                let _ = m.lock().await;
            }
            value_copy.set(());
        })
        .unwrap();
        tasks.push(value)
    }

    for t in tasks {
        t.await;
    }
}
