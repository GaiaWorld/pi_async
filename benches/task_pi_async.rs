#![feature(test)]

extern crate test;

use pi_async::prelude::{SingleTaskPool, SingleTaskRunner, SingleTaskRuntime};
use pi_async::rt::serial_local_thread::{LocalTaskRunner, LocalTaskRuntime};
use pi_async::rt::AsyncRuntimeExt;
use test::Bencher;

fn runtime() -> LocalTaskRuntime {
    LocalTaskRunner::<()>::new().into_local()
}

#[bench]
fn local_block_on(b: &mut Bencher) {
    let rt = runtime();
    b.iter(|| rt.block_on(async {}));
}

fn runtime_single() -> SingleTaskRuntime {
    let pool = SingleTaskPool::default();
    SingleTaskRunner::<(), SingleTaskPool<()>>::new(pool).into_local()
}

#[bench]
fn single_block_on(b: &mut Bencher) {
    let rt = runtime_single();
    b.iter(|| rt.block_on(async {}));
}
