#![feature(test)]
extern crate test;

use std::sync::Arc;
use std::time::Duration;
use test::Bencher;

use pi_async::lock::mutex_lock::Mutex;
use pi_async::rt::multi_thread::{MultiTaskRuntime, MultiTaskRuntimeBuilder};
use pi_async::rt::AsyncRuntime;

use crossbeam_channel::bounded;

#[bench]
fn bench_async_mutex(b: &mut Bencher) {
    let pool0 = MultiTaskRuntimeBuilder::default();
    let rt0: MultiTaskRuntime<()> = pool0.build();

    let pool1 = MultiTaskRuntimeBuilder::default();
    let rt1: MultiTaskRuntime<()> = pool1.build();

    b.iter(|| {
        let rt0_copy = rt0.clone();
        let rt1_copy = rt1.clone();

        let (s, r) = bounded(1);

        let shared = Arc::new(Mutex::new(0));

        for _ in 0..1 {
            let s0_copy = s.clone();
            let shared0_copy = shared.clone();
            rt0_copy
                .spawn(rt0_copy.alloc(), async move {
                    for _ in 0..500 {
                        let mut v = shared0_copy.lock().await;
                        if *v >= 999 {
                            *v += 1;
                            s0_copy.send(()).unwrap();
                        } else {
                            *v += 1;
                        }
                    }
                })
                .unwrap();
        }

        for _ in 0..1 {
            let s1_copy = s.clone();
            let shared1_copy = shared.clone();
            rt1_copy
                .spawn(rt1_copy.alloc(), async move {
                    for _ in 0..500 {
                        let mut v = shared1_copy.lock().await;
                        if *v >= 999 {
                            *v += 1;
                            s1_copy.send(()).unwrap();
                        } else {
                            *v += 1;
                        }
                    }
                })
                .unwrap();
        }

        if let Err(_) = r.recv_timeout(Duration::from_millis(10000)) {
            println!("!!!!!!recv timeout, len: {:?}", (rt0.len(), rt1.len()));
        }
    });
}
