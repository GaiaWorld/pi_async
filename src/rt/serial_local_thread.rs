use std::thread;
use std::rc::Rc;
use std::io::Result;
use std::future::Future;
use std::cell::UnsafeCell;
use std::task::{Context, Poll};
use std::collections::VecDeque;
use std::io::{Error, Result as IOResult, ErrorKind};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use futures::{task::{ArcWake, waker_ref},
              future::{FutureExt, LocalBoxFuture},
              stream::{StreamExt, Stream, LocalBoxStream}};
use async_stream::stream;
use crossbeam_channel::bounded;
use crossbeam_queue::SegQueue;
use flume::bounded as async_bounded;

use crate::{lock::spin,
            rt::{AsyncPipelineResult, alloc_rt_uid,
                 serial::{AsyncWait, AsyncWaitAny, AsyncWaitAnyCallback, AsyncMapReduce}}};

// 本地异步任务
pub(crate) struct LocalTask<O: Default + 'static = ()> {
    inner:      UnsafeCell<Option<LocalBoxFuture<'static, O>>>, //内部本地异步任务
    runtime:    LocalTaskRuntime<O>,                            //本地异步任务运行时
}

unsafe impl<O: Default + 'static> Send for LocalTask<O> {}
unsafe impl<O: Default + 'static> Sync for LocalTask<O> {}

impl<O: Default + 'static> ArcWake for LocalTask<O> {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.runtime.will_wakeup_once(arc_self.clone());
    }
}

impl<O: Default + 'static> LocalTask<O> {
    // 获取内部本地异步任务
    pub fn get_inner(&self) -> Option<LocalBoxFuture<'static, O>> {
        unsafe {
            (&mut *self.inner.get()).take()
        }
    }

    // 设置内部本地异步任务
    pub fn set_inner(&self, inner: Option<LocalBoxFuture<'static, O>>) {
        unsafe {
            *self.inner.get() = inner;
        }
    }
}

///
/// 本地异步任务运行时
///
pub struct LocalTaskRuntime<O: Default + 'static = ()>(Arc<(
    usize,                                      //运行时唯一id
    Arc<AtomicBool>,                            //运行状态
    SegQueue<Arc<LocalTask<O>>>,                //待唤醒本地异步任务队列
    UnsafeCell<VecDeque<Arc<LocalTask<O>>>>,    //本地异步任务池
)>);

unsafe impl<O: Default + 'static> Send for LocalTaskRuntime<O> {}
impl<O: Default + 'static> !Sync for LocalTaskRuntime<O> {}

impl<O: Default + 'static> Clone for LocalTaskRuntime<O> {
    fn clone(&self) -> Self {
        LocalTaskRuntime(self.0.clone())
    }
}

impl<O: Default + 'static> LocalTaskRuntime<O> {
    /// 判断当前本地异步任务运行时是否正在运行
    #[inline]
    pub fn is_running(&self) -> bool {
        (self.0).1.load(Ordering::Relaxed)
    }

    /// 获取当前异步运行时的唯一id
    pub fn get_id(&self) -> usize {
        (self.0).0
    }

    /// 获取当前异步运行时任务数量
    pub fn len(&self) -> usize {
        unsafe {
            (&*(self.0).3.get()).len()
        }
    }

    /// 派发一个指定的异步任务到异步运行时
    pub fn spawn<F>(&self, future: F)
        where F: Future<Output = O> + 'static {
        unsafe {
            (&mut *(self.0).3.get()).push_back(Arc::new(LocalTask {
                inner: UnsafeCell::new(Some(future.boxed_local())),
                runtime: self.clone(),
            }));
        }
    }

    /// 线程安全的发送一个异步任务到异步运行时
    pub fn send<F>(&self, future: F)
        where F: Future<Output = O> + 'static {
        self.will_wakeup_once(Arc::new(LocalTask {
            inner: UnsafeCell::new(Some(future.boxed_local())),
            runtime: self.clone(),
        }));
    }

    /// 线程安全的唤醒一个将被唤醒的本地异步任务
    #[inline]
    pub fn wakeup_once(&self) {
        if let Some(task) = (self.0).2.pop() {
            unsafe {
                (&mut *(self.0).3.get()).push_back(task);
            }
        }
    }

    /// 线程安全的将一个挂起的本地异步任务设置为将被唤醒的本地异步任务
    #[inline]
    fn will_wakeup_once(&self, task: Arc<LocalTask<O>>) {
        (self.0).2.push(task);
    }

    /// 挂起当前异步运行时的当前任务，并在指定的其它运行时上派发一个指定的异步任务，等待其它运行时上的异步任务完成后，唤醒当前运行时的当前任务，并返回其它运行时上的异步任务的值
    fn wait<V: 'static>(&self) -> AsyncWait<V> {
        AsyncWait::new(self.wait_any(2))
    }

    /// 挂起当前异步运行时的当前任务，并在多个其它运行时上执行多个其它任务，其中任意一个任务完成，则唤醒当前运行时的当前任务，并返回这个已完成任务的值，而其它未完成的任务的值将被忽略
    fn wait_any<V: 'static>(&self, capacity: usize) -> AsyncWaitAny<V> {
        let (producor, consumer) = async_bounded(capacity);

        AsyncWaitAny::new(capacity, producor, consumer)
    }

    /// 挂起当前异步运行时的当前任务，并在多个其它运行时上执行多个其它任务，任务返回后需要通过用户指定的检查回调进行检查，其中任意一个任务检查通过，则唤醒当前运行时的当前任务，并返回这个已完成任务的值，而其它未完成或未检查通过的任务的值将被忽略，如果所有任务都未检查通过，则强制唤醒当前运行时的当前任务
    fn wait_any_callback<V: 'static>(&self, capacity: usize) -> AsyncWaitAnyCallback<V> {
        let (producor, consumer) = async_bounded(capacity);

        AsyncWaitAnyCallback::new(capacity, producor, consumer)
    }

    /// 构建用于派发多个异步任务到指定运行时的映射归并，需要指定映射归并的容量
    fn map_reduce<V: 'static>(&self, capacity: usize) -> AsyncMapReduce<V> {
        let (producor, consumer) = async_bounded(capacity);

        AsyncMapReduce::new(0, capacity, producor, consumer)
    }

    /// 生成一个异步管道，输入指定流，输入流的每个值通过过滤器生成输出流的值
    pub fn pipeline<S, SO, F, FO>(&self, input: S, mut filter: F) -> LocalBoxStream<'static, FO>
        where S: Stream<Item = SO> + 'static,
              SO: 'static,
              F: FnMut(SO) -> AsyncPipelineResult<FO> + 'static,
              FO: 'static {
        let output = stream! {
            for await value in input {
                match filter(value) {
                    AsyncPipelineResult::Disconnect => {
                        //立即中止管道
                        break;
                    },
                    AsyncPipelineResult::Filtered(result) => {
                        yield result;
                    },
                }
            }
        };

        output.boxed_local()
    }

    fn block_on<F>(&self, future: F) -> IOResult<F::Output>
        where F: Future + 'static,
              <F as Future>::Output: Default + 'static {
        let (sender, receiver) = bounded(1);
        self.spawn(async move {
            //在指定运行时中执行，并返回结果
            let r = future.await;
            sender.send(r);

            Default::default()
        });
        self.wakeup_once();

        let mut count = 0;
        let mut spin_len = 1;
        loop {
            count += 1;
            if count > 3 {
                //当前异步任务执行时间过长，则自旋后继续等待
                spin_len = spin(spin_len);
            }

            //在本地线程中推动当前运行时执行
            unsafe {
                let option = (&mut *(self.0).3.get()).pop_front();
                if let Some(task) = option {
                    let waker = waker_ref(&task);
                    let mut context = Context::from_waker(&*waker);
                    if let Some(mut future) = task.get_inner() {
                        if let Poll::Pending = future.as_mut().poll(&mut context) {
                            //当前未准备好，则恢复本地异步任务，以保证本地异步任务不被提前释放
                            task.set_inner(Some(future));
                        }
                    }
                }
            }

            //尝试获取异步任务的执行结果
            match receiver.try_recv() {
                Err(e) => {
                    if e.is_disconnected() {
                        //通道已关闭，则立即返回错误原因
                        return Err(Error::new(ErrorKind::Other, format!("Block on failed, reason: {:?}", e)));
                    }
                },
                Ok(result) => {
                    //异步任务已完成，则立即返回执行结果
                    return Ok(result)
                },
            }
        }
    }

    /// 关闭异步运行时，返回请求关闭是否成功
    pub fn close(self) -> bool {
        if cfg!(target_arch = "aarch64") {
            if let Ok(true) = (self.0).1.compare_exchange(true,
                                                          false,
                                                          Ordering::SeqCst,
                                                          Ordering::SeqCst) {
                //设置运行状态成功
                true
            } else {
                false
            }
        } else {
            if let Ok(true) = (self.0).1.compare_exchange_weak(true,
                                                               false,
                                                               Ordering::SeqCst,
                                                               Ordering::SeqCst) {
                //设置运行状态成功
                true
            } else {
                false
            }
        }
    }
}

///
/// 本地异步任务执行器
///
pub struct LocalTaskRunner<O: Default + 'static = ()>(LocalTaskRuntime<O>);

unsafe impl<O: Default + 'static> Send for LocalTaskRunner<O> {}
impl<O: Default + 'static> !Sync for LocalTaskRunner<O> {}

impl<O: Default + 'static> LocalTaskRunner<O> {
    /// 构建本地异步任务执行器
    pub fn new() -> Self {
        let inner = (
                alloc_rt_uid(),
                Arc::new(AtomicBool::new(false)),
                SegQueue::new(),
                UnsafeCell::new(VecDeque::new()),
        );

        LocalTaskRunner(LocalTaskRuntime(Arc::new(inner)))
    }

    /// 获取当前本地异步任务执行器的运行时
    pub fn get_runtime(&self) -> LocalTaskRuntime<O> {
        self.0.clone()
    }

    /// 启动工作者异步任务执行器
    pub fn startup(self,
                   thread_name: &str,
                   thread_stack_size: usize) -> LocalTaskRuntime<O> {
        let rt = self.get_runtime();
        let rt_copy = rt.clone();
        thread::Builder::new()
            .name(thread_name.to_string())
            .stack_size(thread_stack_size)
            .spawn(move || {
                (rt_copy.0).1.store(true, Ordering::Relaxed);

                while rt_copy.is_running() {
                    rt_copy.wakeup_once();
                    self.run_once();
                }
            });

        rt
    }

    // 运行一次本地异步任务执行器
    #[inline]
    pub fn run_once(&self) {
        unsafe {
            let option = (&mut *((self.0).0).3.get()).pop_front();
            if let Some(task) = option {
                let waker = waker_ref(&task);
                let mut context = Context::from_waker(&*waker);
                if let Some(mut future) = task.get_inner() {
                    if let Poll::Pending = future.as_mut().poll(&mut context) {
                        //当前未准备好，则恢复本地异步任务，以保证本地异步任务不被提前释放
                        task.set_inner(Some(future));
                    }
                }
            }
        }
    }

    /// 转换为本地异步任务运行时
    pub fn into_local(self) -> LocalTaskRuntime<O> {
        self.0
    }
}

#[test]
fn test_local_runtime_block_on() {
    use std::time::Instant;
    use std::ops::Drop;
    use std::sync::atomic::AtomicUsize;

    struct AtomicCounter(AtomicUsize, Instant);
    impl Drop for AtomicCounter {
        fn drop(&mut self) {
            unsafe {
                println!("!!!!!!drop counter, count: {:?}, time: {:?}", self.0.load(Ordering::Relaxed), Instant::now() - self.1);
            }
        }
    }

    let rt = LocalTaskRunner::<()>::new().into_local();

    let counter = Arc::new(AtomicCounter(AtomicUsize::new(0), Instant::now()));
    let start = Instant::now();
    for _ in 0..10000000 {
        let counter_copy = counter.clone();
        let _ = rt.block_on(async move {
            counter_copy.0.fetch_add(1, Ordering::Relaxed)
        });
    }
    println!("!!!!!!spawn local task ok, time: {:?}", Instant::now() - start);
}
