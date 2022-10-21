//! # 提供了通用的异步运行时
//!

use std::thread;
use std::rc::Rc;
use std::pin::Pin;
use std::vec::IntoIter;
use std::future::Future;
use std::mem::transmute;
use std::sync::{Arc, Weak};
use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::ptr::{null, null_mut};
use std::cell::{RefCell, UnsafeCell};
use std::panic::{PanicInfo, set_hook};
use std::task::{Waker, Context, Poll};
use std::io::{Error, Result, ErrorKind};
use std::alloc::{Layout, set_alloc_error_hook};
use std::time::{Duration, SystemTime};
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, AtomicIsize, AtomicPtr, Ordering};

pub mod single_thread;
pub mod multi_thread;
pub mod worker_thread;
pub mod async_pipeline;
pub mod serial;
pub mod serial_local_thread;
pub mod serial_single_thread;
pub mod serial_worker_thread;

use libc;
use futures::{future::{FutureExt, BoxFuture},
              stream::{Stream, BoxStream},
              task::ArcWake};
use parking_lot::{Mutex, Condvar};
use crossbeam_channel::{Sender, Receiver, unbounded};
use crossbeam_queue::ArrayQueue;
use flume::{Sender as AsyncSender, Receiver as AsyncReceiver, bounded as async_bounded};
use num_cpus;
use backtrace::Backtrace;
use slotmap::{Key, KeyData};

use pi_time::Instant;
use pi_hash::XHashMap;
use pi_cancel_timer::Timer;

use single_thread::{SingleTaskPool, SingleTaskRunner, SingleTaskRuntime};
use multi_thread::{StealableTaskPool, MultiTaskRuntimeBuilder, MultiTaskRuntime};
use worker_thread::{WorkerTaskRunner, WorkerRuntime};

use crate::lock::spin;
use crate::lock::spin_lock::SpinLock;

/*
* 线程绑定的异步运行时
*/
thread_local! {
    static PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME: AtomicPtr<()> = AtomicPtr::new(null_mut());
    static PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT: UnsafeCell<XHashMap<TypeId, Box<dyn Any + 'static>>> = UnsafeCell::new(XHashMap::default());
}

/*
* 异步运行时唯一id生成器
*/
static RUNTIME_UID_GEN: AtomicUsize = AtomicUsize::new(1);

///
/// 分配异步运行时唯一id
///
pub fn alloc_rt_uid() -> usize {
    RUNTIME_UID_GEN.fetch_add(1, Ordering::Relaxed)
}

///
/// 异步任务唯一id
///
#[derive(Clone)]
pub struct TaskId(Arc<AtomicUsize>);

impl Debug for TaskId {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(f, "TaskId[inner = {}]", self.0.load(Ordering::Relaxed))
    }
}

///
/// 异步任务
///
pub struct AsyncTask<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static = (),
> {
    uid:        TaskId,                                 //任务唯一id
    future:     Mutex<Option<BoxFuture<'static, O>>>,   //异步任务
    pool:       Arc<P>,                                 //异步任务池
    context:    Option<UnsafeCell<Box<dyn Any>>>,       //异步任务上下文
}

unsafe impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Send for AsyncTask<P, O> {}
unsafe impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Sync for AsyncTask<P, O> {}

impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = P>,
    O: Default + 'static,
> ArcWake for AsyncTask<P, O> {
    #[cfg(not(target_arch = "aarch64"))]
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let pool = arc_self.get_pool();
        let _ = pool.push_keep(arc_self.clone());

        if let Some(waits) = pool.get_waits() {
            //当前任务属于多线程异步运行时
            if let Some(worker_waker) = waits.pop() {
                //有待唤醒的工作者
                let (is_sleep, lock, condvar) = &*worker_waker;
                let locked = lock.lock();
                if is_sleep.load(Ordering::Relaxed) {
                    //待唤醒的工作者，正在休眠，则立即唤醒此工作者
                    if let Ok(true) = is_sleep
                        .compare_exchange_weak(true,
                                               false,
                                               Ordering::SeqCst,
                                               Ordering::SeqCst) {
                        //确认需要唤醒，则唤醒
                        condvar.notify_one();
                    }
                }
            }
        } else {
            //当前线程属于单线程异步运行时
            if let Some(thread_waker) = pool.get_thread_waker() {
                //当前任务池绑定了所在线程的唤醒器，则快速检查是否需要唤醒所在线程
                if thread_waker.0.load(Ordering::Relaxed) {
                    let (is_sleep, lock, condvar) = &**thread_waker;
                    let locked = lock.lock();
                    //待唤醒的线程，正在休眠，则立即唤醒此线程
                    if let Ok(true) = is_sleep
                        .compare_exchange_weak(true,
                                               false,
                                               Ordering::SeqCst,
                                               Ordering::SeqCst) {
                        //确认需要唤醒，则唤醒
                        condvar.notify_one();
                    }
                }
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let pool = arc_self.get_pool();
        let _ = pool.push_keep(arc_self.clone());

        if let Some(waits) = pool.get_waits() {
            //当前任务属于多线程异步运行时
            if let Some(worker_waker) = waits.pop() {
                //有待唤醒的工作者
                let (is_sleep, lock, condvar) = &*worker_waker;
                let locked = lock.lock();
                if is_sleep.load(Ordering::Relaxed) {
                    //待唤醒的工作者，正在休眠，则立即唤醒此工作者
                    if let Ok(true) = is_sleep
                        .compare_exchange(true,
                                          false,
                                          Ordering::SeqCst,
                                          Ordering::SeqCst) {
                        //确认需要唤醒，则唤醒
                        condvar.notify_one();
                    }
                }
            }
        } else {
            //当前线程属于单线程异步运行时
            if let Some(thread_waker) = pool.get_thread_waker() {
                //当前任务池绑定了所在线程的唤醒器，则快速检查是否需要唤醒所在线程
                if thread_waker.0.load(Ordering::Relaxed) {
                    let (is_sleep, lock, condvar) = &**thread_waker;
                    let locked = lock.lock();
                    //待唤醒的线程，正在休眠，则立即唤醒此线程
                    if let Ok(true) = is_sleep
                        .compare_exchange(true,
                                          false,
                                          Ordering::SeqCst,
                                          Ordering::SeqCst) {
                        //确认需要唤醒，则唤醒
                        condvar.notify_one();
                    }
                }
            }
        }
    }
}

impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = P>,
    O: Default + 'static,
> AsyncTask<P, O> {
    /// 构建单线程任务
    pub fn new(uid: TaskId,
               pool: Arc<P>,
               future: Option<BoxFuture<'static, O>>) -> AsyncTask<P, O> {
        AsyncTask {
            uid,
            future: Mutex::new(future),
            pool,
            context: None,
        }
    }

    /// 使用指定上下文构建单线程任务
    pub fn with_context<C: 'static>(uid: TaskId,
                                    pool: Arc<P>,
                                    future: Option<BoxFuture<'static, O>>,
                                    context: C) -> AsyncTask<P, O> {
        let any = Box::new(context);

        AsyncTask {
            uid,
            future: Mutex::new(future),
            pool,
            context: Some(UnsafeCell::new(any)),
        }
    }

    /// 使用指定异步运行时和上下文构建单线程任务
    pub fn with_runtime_and_context<RT, C>(runtime: &RT,
                                           future: Option<BoxFuture<'static, O>>,
                                           context: C) -> AsyncTask<P, O>
        where RT: AsyncRuntime<O, Pool = P>,
              C: 'static {
        let any = Box::new(context);

        AsyncTask {
            uid: runtime.alloc(),
            future: Mutex::new(future),
            pool: runtime.shared_pool(),
            context: Some(UnsafeCell::new(any)),
        }
    }

    /// 检查是否允许唤醒
    pub fn is_enable_wakeup(&self) -> bool {
        self.uid.0.load(Ordering::Relaxed) > 0
    }

    /// 获取内部任务
    pub fn get_inner(&self) -> Option<BoxFuture<'static, O>> {
        self.future.lock().take()
    }

    /// 设置内部任务
    pub fn set_inner(&self, inner: Option<BoxFuture<'static, O>>) {
        *self.future.lock() = inner;
    }

    //判断异步任务是否有上下文
    pub fn exist_context(&self) -> bool {
        self.context.is_some()
    }

    //获取异步任务上下文的只读引用
    pub fn get_context<C: 'static>(&self) -> Option<&C> {
        if let Some(context) = &self.context {
            //存在上下文
            let any = unsafe { &*context.get() };
            return <dyn Any>::downcast_ref::<C>(&**any);
        }

        None
    }

    //获取异步任务上下文的可写引用
    pub fn get_context_mut<C: 'static>(&self) -> Option<&mut C> {
        if let Some(context) = &self.context {
            //存在上下文
            let any = unsafe { &mut *context.get() };
            return <dyn Any>::downcast_mut::<C>(&mut **any);
        }

        None
    }

    //设置异步任务上下文，返回上一个异步任务上下文
    pub fn set_context<C: 'static>(&self, new: C) {
        if let Some(context) = &self.context {
            //存在上一个上下文，则释放上一个上下文
            let _ = unsafe { &*context.get() };

            //设置新的上下文
            let any: Box<dyn Any + 'static> = Box::new(new);
            unsafe { *context.get() = any; }
        }
    }

    //获取异步任务的任务池
    pub fn get_pool(&self) -> &P {
        self.pool.as_ref()
    }
}

///
/// 异步任务池
///
pub trait AsyncTaskPool<O: Default + 'static = ()>: Default + Send + Sync + 'static {
    type Pool: AsyncTaskPoolExt<O> + AsyncTaskPool<O>;

    /// 获取绑定的线程唯一id
    fn get_thread_id(&self) -> usize;

    /// 获取当前异步任务池内任务数量
    fn len(&self) -> usize;

    /// 将异步任务加入异步任务池
    fn push(&self, task: Arc<AsyncTask<Self::Pool, O>>) -> Result<()>;

    /// 将已超时的异步任务加入任务池
    fn push_timed_out(&self, index: u64, task: Arc<AsyncTask<Self::Pool, O>>) -> Result<()>;

    /// 异步任务被唤醒时，将异步任务继续加入异步任务池
    fn push_keep(&self, task: Arc<AsyncTask<Self::Pool, O>>) -> Result<()>;

    /// 尝试从异步任务池中弹出一个异步任务
    fn try_pop(&self) -> Option<Arc<AsyncTask<Self::Pool, O>>>;

    /// 尝试从异步任务池中弹出所有异步任务
    fn try_pop_all(&self) -> IntoIter<Arc<AsyncTask<Self::Pool, O>>>;

    /// 获取本地线程的唤醒器
    fn get_thread_waker(&self) -> Option<&Arc<(AtomicBool, Mutex<()>, Condvar)>>;
}

///
/// 异步任务池扩展
///
pub trait AsyncTaskPoolExt<O: Default + 'static = ()>: Send + Sync + 'static {
    /// 设置待唤醒的工作者唤醒器队列
    fn set_waits(&mut self,
                 _waits: Arc<ArrayQueue<Arc<(AtomicBool, Mutex<()>, Condvar)>>>) {}

    /// 获取待唤醒的工作者唤醒器队列
    fn get_waits(&self) -> Option<&Arc<ArrayQueue<Arc<(AtomicBool, Mutex<()>, Condvar)>>>> {
        //默认没有待唤醒的工作者唤醒器队列
        None
    }

    /// 获取空闲的工作者的数量，这个数量大于0，表示可以新开线程来运行可分派的工作者
    fn idler_len(&self) -> usize {
        //默认不分派
        0
    }

    /// 分派一个空闲的工作者
    fn spawn_worker(&self) -> Option<usize> {
        //默认不分派
        None
    }

    /// 获取工作者的数量
    fn worker_len(&self) -> usize {
        //默认工作者数量和本机逻辑核数相同
        #[cfg(not(target_arch = "wasm32"))]
        return num_cpus::get();
        #[cfg(target_arch = "wasm32")]
        return 1;
    }

    /// 获取缓冲区的任务数量，缓冲区任务是未分配给工作者的任务
    fn buffer_len(&self) -> usize {
        //默认没有缓冲区
        0
    }

    /// 设置当前绑定本地线程的唤醒器
    fn set_thread_waker(&mut self, _thread_waker: Arc<(AtomicBool, Mutex<()>, Condvar)>) {
        //默认不设置
    }

    /// 复制当前绑定本地线程的唤醒器
    fn clone_thread_waker(&self) -> Option<Arc<(AtomicBool, Mutex<()>, Condvar)>> {
        //默认不复制
        None
    }

    /// 关闭当前工作者
    fn close_worker(&self) {
        //默认不允许关闭工作者
    }
}

///
/// 异步运行时
///
pub trait AsyncRuntime<O: Default + 'static = ()>: Clone + Send + Sync + 'static {
    type Pool: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = Self::Pool>;

    /// 共享运行时内部任务池
    fn shared_pool(&self) -> Arc<Self::Pool>;

    /// 获取当前异步运行时的唯一id
    fn get_id(&self) -> usize;

    /// 获取当前异步运行时待处理任务数量
    fn wait_len(&self) -> usize;

    /// 获取当前异步运行时任务数量
    fn len(&self) -> usize;

    /// 分配异步任务的唯一id
    fn alloc(&self) -> TaskId;

    /// 派发一个指定的异步任务到异步运行时
    fn spawn<F>(&self, task_id: TaskId, future: F) -> Result<()>
        where F: Future<Output = O> + Send + 'static;

    /// 派发一个在指定时间后执行的异步任务到异步运行时，时间单位ms
    fn spawn_timing<F>(&self, task_id: TaskId, future: F, time: usize) -> Result<()>
        where F: Future<Output = O> + Send + 'static;

    /// 挂起指定唯一id的异步任务
    fn pending<Output>(&self, task_id: &TaskId, waker: Waker) -> Poll<Output>;

    /// 唤醒指定唯一id的异步任务
    fn wakeup(&self, task_id: &TaskId);

    /// 挂起当前异步运行时的当前任务，并在指定的其它运行时上派发一个指定的异步任务，等待其它运行时上的异步任务完成后，唤醒当前运行时的当前任务，并返回其它运行时上的异步任务的值
    fn wait<V: Send + 'static>(&self) -> AsyncWait<V>;

    /// 挂起当前异步运行时的当前任务，并在多个其它运行时上执行多个其它任务，其中任意一个任务完成，则唤醒当前运行时的当前任务，并返回这个已完成任务的值，而其它未完成的任务的值将被忽略
    fn wait_any<V: Send + 'static>(&self, capacity: usize) -> AsyncWaitAny<V>;

    /// 挂起当前异步运行时的当前任务，并在多个其它运行时上执行多个其它任务，任务返回后需要通过用户指定的检查回调进行检查，其中任意一个任务检查通过，则唤醒当前运行时的当前任务，并返回这个已完成任务的值，而其它未完成或未检查通过的任务的值将被忽略，如果所有任务都未检查通过，则强制唤醒当前运行时的当前任务
    fn wait_any_callback<V: Send + 'static>(&self, capacity: usize) -> AsyncWaitAnyCallback<V>;

    /// 构建用于派发多个异步任务到指定运行时的映射归并，需要指定映射归并的容量
    fn map_reduce<V: Send + 'static>(&self, capacity: usize) -> AsyncMapReduce<V>;

    /// 挂起当前异步运行时的当前任务，等待指定的时间后唤醒当前任务
    fn timeout(&self, timeout: usize) -> BoxFuture<'static, ()>;

    /// 生成一个异步管道，输入指定流，输入流的每个值通过过滤器生成输出流的值
    fn pipeline<S, SO, F, FO>(&self, input: S, filter: F) -> BoxStream<'static, FO>
        where S: Stream<Item = SO> + Send + 'static,
              SO: Send + 'static,
              F: FnMut(SO) -> AsyncPipelineResult<FO> + Send + 'static,
              FO: Send + 'static;

    /// 关闭异步运行时，返回请求关闭是否成功
    fn close(&self) -> bool;
}

///
/// 异步运行时扩展
///
pub trait AsyncRuntimeExt<O: Default + 'static = ()> {
    /// 派发一个指定的异步任务到异步运行时，并指定异步任务的初始化上下文
    fn spawn_with_context<F, C>(&self,
                                task_id: TaskId,
                                future: F,
                                context: C) -> Result<()>
        where F: Future<Output = O> + Send + 'static,
              C: 'static;

    /// 派发一个在指定时间后执行的异步任务到异步运行时，并指定异步任务的初始化上下文，时间单位ms
    fn spawn_timing_with_context<F, C>(&self,
                                       task_id: TaskId,
                                       future: F,
                                       context: C,
                                       time: usize) -> Result<()>
        where F: Future<Output = O> + Send + 'static,
              C: 'static;

    /// 立即创建一个指定任务池的异步运行时，并执行指定的异步任务，阻塞当前线程，等待异步任务完成后返回
    fn block_on<RP, F>(&self, future: F) -> Result<F::Output>
        where RP: AsyncTaskPoolExt<F::Output> + AsyncTaskPool<F::Output, Pool = RP>,
              F: Future + Send + 'static,
              <F as Future>::Output: Default + Send + 'static;
}

///
/// 异步运行时构建器
///
pub struct AsyncRuntimeBuilder<O: Default + 'static = ()>(PhantomData<O>);

impl<O: Default + 'static> AsyncRuntimeBuilder<O> {
    /// 构建默认的工作者异步运行时
    pub fn default_worker_thread(worker_name: Option<&str>,
                                 worker_stack_size: Option<usize>,
                                 worker_sleep_timeout: Option<u64>,
                                 worker_loop_interval: Option<Option<u64>>) -> WorkerRuntime<O> {
        let runner = WorkerTaskRunner::default();

        let thread_name = if let Some(name) = worker_name {
            name
        } else {
            //默认的线程名称
            "Default-Single-Worker"
        };
        let thread_stack_size = if let Some(size) = worker_stack_size {
            size
        } else {
            //默认的线程堆栈大小
            2 * 1024 * 1024
        };
        let sleep_timeout = if let Some(timeout) = worker_sleep_timeout {
            timeout
        } else {
            //默认的线程休眠时长
            1
        };
        let loop_interval = if let Some(interval) = worker_loop_interval {
            interval
        } else {
            //默认的线程循环间隔时长
            None
        };

        //创建线程并在线程中执行异步运行时
        let runner_copy = runner.clone();
        let rt_copy = runner.get_runtime();
        let rt = runner.startup(
            thread_name,
            thread_stack_size,
            sleep_timeout,
            loop_interval,
            move || {
                let now = Instant::now();
                match runner_copy.run_once() {
                    Err(e) => {
                        panic!("Run runner failed, reason: {:?}", e);
                    },
                    Ok(len) => {
                        (len == 0, Instant::now().duration_since(now))
                    },
                }
            },
            move || {
                rt_copy.wait_len() + rt_copy.len()
            },
        );

        rt
    }

    /// 构建自定义的工作者异步运行时
    pub fn custom_worker_thread<P, F0, F1>(pool: P,
                                           worker_handle: Arc<AtomicBool>,
                                           worker_condvar: Arc<(AtomicBool, Mutex<()>, Condvar)>,
                                           thread_name: &str,
                                           thread_stack_size: usize,
                                           sleep_timeout: u64,
                                           loop_interval: Option<u64>,
                                           loop_func: F0,
                                           get_queue_len: F1) -> WorkerRuntime<O, P>
        where P: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = P>,
              F0: Fn() -> (bool, Duration) + Send + 'static,
              F1: Fn() -> usize + Send + 'static {
        let runner = WorkerTaskRunner::new(pool,
                                           worker_handle,
                                           worker_condvar);

        //创建线程并在线程中执行异步运行时
        let rt_copy = runner.get_runtime();
        let rt = runner.startup(
            thread_name,
            thread_stack_size,
            sleep_timeout,
            loop_interval,
            loop_func,
            move || {
                rt_copy.wait_len() + get_queue_len()
            },
        );

        rt
    }

    /// 构建默认的多线程异步运行时
    pub fn default_multi_thread(worker_prefix: Option<&str>,
                                worker_stack_size: Option<usize>,
                                worker_size: Option<usize>,
                                worker_sleep_timeout: Option<u64>) -> MultiTaskRuntime<O> {
        let mut builder = MultiTaskRuntimeBuilder::default();

        if let Some(thread_prefix) = worker_prefix {
            builder = builder.thread_prefix(thread_prefix);
        }
        if let Some(thread_stack_size) = worker_stack_size {
            builder = builder.thread_stack_size(thread_stack_size);
        }
        if let Some(size) = worker_size {
            builder = builder
                .init_worker_size(size)
                .set_worker_limit(size, size);
        }
        if let Some(sleep_timeout) = worker_sleep_timeout {
            builder = builder.set_timeout(sleep_timeout);
        }

        builder.build()
    }

    /// 构建自定义的多线程异步运行时
    pub fn custom_multi_thread<P>(pool: P,
                                  worker_prefix: &str,
                                  worker_stack_size: usize,
                                  worker_size: usize,
                                  worker_sleep_timeout: u64,
                                  worker_timer_interval: usize) -> MultiTaskRuntime<O, P>
        where P: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = P> {
        MultiTaskRuntimeBuilder::new(pool)
            .thread_prefix(worker_prefix)
            .thread_stack_size(worker_stack_size)
            .init_worker_size(worker_size)
            .set_worker_limit(worker_size, worker_size)
            .set_timeout(worker_sleep_timeout)
            .set_timer_interval(worker_timer_interval)
            .build()
    }
}

/// 绑定指定异步运行时到本地线程
pub fn bind_local_thread<O: Default + 'static>(runtime: LocalAsyncRuntime<O>) {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME.try_with(move |rt| {
        let raw = Arc::into_raw(Arc::new(runtime)) as *mut LocalAsyncRuntime<O> as *mut ();
        rt.store(raw, Ordering::Relaxed);
    }) {
        Err(e) => {
            panic!("Bind single runtime to local thread failed, reason: {:?}", e);
        },
        Ok(_) => (),
    }
}

/// 从本地线程解绑单线程异步任务执行器
pub fn unbind_local_thread() {
    let _ = PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME.try_with(move |rt| {
        rt.store(null_mut(), Ordering::Relaxed);
    });
}

///
/// 本地线程绑定的异步运行时
///
pub struct LocalAsyncRuntime<O: Default + 'static> {
    inner:              *const (),                                                  //内部运行时指针
    get_id_func:        fn(*const ()) -> usize,                                     //获取本地运行时的id的函数
    spawn_func:         fn(*const (), BoxFuture<'static, O>) -> Result<()>,         //派发函数
    spawn_timing_func:  fn(*const (), BoxFuture<'static, O>, usize) -> Result<()>,  //定时派发函数
    timeout_func:       fn(*const (), usize) -> BoxFuture<'static, ()>,             //超时函数
}

unsafe impl<O: Default + 'static> Send for LocalAsyncRuntime<O> {}
unsafe impl<O: Default + 'static> Sync for LocalAsyncRuntime<O> {}

impl<O: Default + 'static> LocalAsyncRuntime<O> {
    /// 创建本地线程绑定的异步运行时
    pub fn new(inner: *const (),
               get_id_func: fn(*const ()) -> usize,
               spawn_func: fn(*const (), BoxFuture<'static, O>) -> Result<()>,
               spawn_timing_func: fn(*const (), BoxFuture<'static, O>, usize) -> Result<()>,
               timeout_func: fn(*const (), usize) -> BoxFuture<'static, ()>) -> Self {
        LocalAsyncRuntime {
            inner,
            get_id_func,
            spawn_func,
            spawn_timing_func,
            timeout_func,
        }
    }

    /// 获取本地运行时的id
    #[inline]
    pub fn get_id(&self) -> usize {
        (self.get_id_func)(self.inner)
    }

    /// 派发一个指定的异步任务到本地线程绑定的异步运行时
    #[inline]
    pub fn spawn<F>(&self, future: F) -> Result<()>
        where F: Future<Output = O> + Send + 'static {
        (self.spawn_func)(self.inner, async move {
            future.await
        }.boxed())
    }

    /// 定时派发一个指定的异步任务到本地线程绑定的异步运行时
    #[inline]
    pub fn sapwn_timing_func<F>(&self, future: F, timeout: usize) -> Result<()>
        where F: Future<Output = O> + Send + 'static {
        (self.spawn_timing_func)(self.inner,
                                 async move {
                                     future.await
                                 }.boxed(),
                                 timeout)
    }

    /// 挂起本地线程绑定的异步运行时的当前任务，等待指定的时间后唤醒当前任务
    #[inline]
    pub fn timeout(&self, timeout: usize) -> BoxFuture<'static, ()> {
        (self.timeout_func)(self.inner, timeout)
    }
}

///
/// 获取本地线程绑定的异步运行时
/// 注意：O如果与本地线程绑定的运行时的O不相同，则无法获取本地线程绑定的运行时
///
pub fn local_async_runtime<O: Default + 'static>() -> Option<Arc<LocalAsyncRuntime<O>>> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME.try_with(move |ptr| {
        let raw = ptr.load(Ordering::Relaxed) as *const LocalAsyncRuntime<O>;
        unsafe {
            if raw.is_null() {
                //本地线程未绑定异步运行时
                None
            } else {
                //本地线程已绑定异步运行时
                let shared: Arc<LocalAsyncRuntime<O>> = unsafe { Arc::from_raw(raw) };
                let result = shared.clone();
                Arc::into_raw(shared); //避免提前释放
                Some(result)
            }
        }
    }) {
        Err(_) => None, //本地线程没有绑定异步运行时
        Ok(rt) => rt,
    }
}

///
/// 派发任务到本地线程绑定的异步运行时，如果本地线程没有异步运行时，则返回错误
/// 注意：F::Output如果与本地线程绑定的运行时的O不相同，则无法执行指定任务
///
pub fn spawn_local<O, F>(future: F) -> Result<()>
    where O: Default + 'static,
          F: Future<Output = O> + Send + 'static {
    if let Some(rt) = local_async_runtime::<O>() {
        rt.spawn(future)
    } else {
        Err(Error::new(ErrorKind::Other, format!("Spawn task to local thread failed, reason: runtime not exist")))
    }
}

///
/// 从本地线程绑定的字典中获取指定类型的值的只读引用
///
pub fn get_local_dict<T: 'static>() -> Option<&'static T> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT.try_with(move |dict| {
        unsafe {
            if let Some(any) = (&*dict.get()).get(&TypeId::of::<T>()) {
                //指定类型的值存在
                <dyn Any>::downcast_ref::<T>(&**any)
            } else {
                //指定类型的值不存在
                None
            }
        }
    }) {
        Err(_) => {
            None
        },
        Ok(result) => {
            result
        }
    }
}

///
/// 从本地线程绑定的字典中获取指定类型的值的可写引用
///
pub fn get_local_dict_mut<T: 'static>() -> Option<&'static mut T> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT.try_with(move |dict| {
        unsafe {
            if let Some(any) = (&mut *dict.get()).get_mut(&TypeId::of::<T>()) {
                //指定类型的值存在
                <dyn Any>::downcast_mut::<T>(&mut **any)
            } else {
                //指定类型的值不存在
                None
            }
        }
    }) {
        Err(_) => {
            None
        },
        Ok(result) => {
            result
        }
    }
}

///
/// 在本地线程绑定的字典中设置指定类型的值，返回上一个设置的值
///
pub fn set_local_dict<T: 'static>(value: T) -> Option<T> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT.try_with(move |dict| {
        unsafe {
            let result = if let Some(any) = (&mut *dict.get()).remove(&TypeId::of::<T>()) {
                //指定类型的上一个值存在
                if let Ok(r) = any.downcast() {
                    //造型成功，则返回
                    Some(*r)
                } else {
                    None
                }
            } else {
                //指定类型的上一个值不存在
                None
            };

            //设置指定类型的新值
            (&mut *dict.get()).insert(TypeId::of::<T>(), Box::new(value) as Box<dyn Any>);

            result
        }
    }) {
        Err(_) => {
            None
        },
        Ok(result) => {
            result
        }
    }
}

///
/// 在本地线程绑定的字典中移除指定类型的值，并返回移除的值
///
pub fn remove_local_dict<T: 'static>() -> Option<T> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT.try_with(move |dict| {
        unsafe {
            if let Some(any) = (&mut *dict.get()).remove(&TypeId::of::<T>()) {
                //指定类型的上一个值存在
                if let Ok(r) = any.downcast() {
                    //造型成功，则返回
                    Some(*r)
                } else {
                    None
                }
            } else {
                //指定类型的上一个值不存在
                None
            }
        }
    }) {
        Err(_) => {
            None
        },
        Ok(result) => {
            result
        }
    }
}

///
/// 清空本地线程绑定的字典
///
pub fn clear_local_dict() -> Result<()> {
    match PI_ASYNC_LOCAL_THREAD_ASYNC_RUNTIME_DICT.try_with(move |dict| {
        unsafe {
            (&mut *dict.get()).clear();
        }
    }) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other, format!("Clear local dict failed, reason: {:?}", e)))
        },
        Ok(_) => {
            Ok(())
        }
    }
}

///
/// 异步值，只允许被设置一次值
///
pub struct AsyncValue<V: Send + 'static>(Arc<InnerAsyncValue<V>>);

unsafe impl<V: Send + 'static> Send for AsyncValue<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncValue<V> {}

impl<V: Send + 'static> Clone for AsyncValue<V> {
    fn clone(&self) -> Self {
        AsyncValue(self.0.clone())
    }
}

impl<V: Send + 'static> Future for AsyncValue<V> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = unsafe { (*(&self).0.value.get()).take() } {
            //异步值已就绪
            return Poll::Ready(value);
        }

        unsafe {
            *self.0.waker.get() = Some(cx.waker().clone()); //设置异步值的唤醒器
        }
        self.0.status.store(1, Ordering::Relaxed); //设置异步值的状态为已就绪
        Poll::Pending
    }
}

/*
* 异步值同步方法
*/
impl<V: Send + 'static> AsyncValue<V> {
    /// 构建异步值，默认值为未就绪
    pub fn new() -> Self {
        let inner = InnerAsyncValue {
            value: UnsafeCell::new(None),
            waker: UnsafeCell::new(None),
            status: AtomicU8::new(0),
        };

        AsyncValue(Arc::new(inner))
    }

    /// 判断异步值是否已完成设置
    pub fn is_complete(&self) -> bool {
        self
            .0
            .status
            .load(Ordering::Relaxed) == 2
    }

    /// 设置异步值
    pub fn set(self, value: V) {
        let mut spin_len = 1;
        loop {
            match self.0.status.compare_exchange(1,
                                                 2,
                                                 Ordering::Acquire,
                                                 Ordering::Relaxed) {
                Err(0) => {
                    //异步值的唤醒器已就绪，则自旋等待
                    spin_len = spin(spin_len);
                },
                Err(_) => {
                    //异步值已被设置，则立即返回
                    return;
                },
                Ok(_) => {
                    //已锁且获取到锁，则立即退出自旋
                    break;
                }
            }
        }

        //已锁且获取到锁，则设置异步值，并立即唤醒异步值
        unsafe { *self.0.value.get() = Some(value); }
        let waker = unsafe { (*self.0.waker.get()).take().unwrap() };
        waker.wake();
    }
}

// 内部异步值，只允许被设置一次值
pub struct InnerAsyncValue<V: Send + 'static> {
    value:  UnsafeCell<Option<V>>,      //值
    waker:  UnsafeCell<Option<Waker>>,  //唤醒器
    status: AtomicU8,                   //状态
}

///
/// 异步可变值的守护者
///
pub struct AsyncVariableGuard<'a, V: Send + 'static> {
    value:  &'a UnsafeCell<Option<V>>,      //值
    waker:  &'a UnsafeCell<Option<Waker>>,  //唤醒器
    status: &'a AtomicU8,                   //值状态
}

unsafe impl<V: Send + 'static> Send for AsyncVariableGuard<'_, V> {}

impl<V: Send + 'static> Drop for AsyncVariableGuard<'_, V> {
    fn drop(&mut self) {
        //将异步可变值的状态从已锁定改为已就绪
        self.status.fetch_sub(2, Ordering::Relaxed);
    }
}

impl<V: Send + 'static> Deref for AsyncVariableGuard<'_, V> {
    type Target = Option<V>;

    fn deref(&self) -> &Self::Target {
        unsafe {
            &*self.value.get()
        }
    }
}

impl<V: Send + 'static> DerefMut for AsyncVariableGuard<'_, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            &mut *self.value.get()
        }
    }
}

impl<V: Send + 'static> AsyncVariableGuard<'_, V> {
    /// 完成异步可变值的修改
    pub fn finish(self) {
        //设置异步可变值的状态为已完成修改
        self.status.fetch_add(4, Ordering::Relaxed);

        //立即唤醒异步可变值
        let waker = unsafe { (&mut *self.waker.get()).take().unwrap() };
        waker.wake();
    }
}

///
/// 异步可变值，在完成前允许被修改多次
///
pub struct AsyncVariable<V: Send + 'static>(Arc<InnerAsyncVariable<V>>);

unsafe impl<V: Send + 'static> Send for AsyncVariable<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncVariable<V> {}

impl<V: Send + 'static> Clone for AsyncVariable<V> {
    fn clone(&self) -> Self {
        AsyncVariable(self.0.clone())
    }
}

impl<V: Send + 'static> Future for AsyncVariable<V> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = unsafe { (&mut *(&self).0.value.get()).take() } {
            //异步可变值已就绪
            return Poll::Ready(value);
        }

        unsafe {
            *self.0.waker.get() = Some(cx.waker().clone()); //设置异步可变值的唤醒器准备就绪
        }
        self.0.status.store(1, Ordering::Release);
        Poll::Pending
    }
}

impl<V: Send + 'static> AsyncVariable<V> {
    /// 构建异步可变值，默认值为未就绪
    pub fn new() -> Self {
        let inner = InnerAsyncVariable {
            value: UnsafeCell::new(None),
            waker: UnsafeCell::new(None),
            status: AtomicU8::new(0),
        };

        AsyncVariable(Arc::new(inner))
    }

    /// 判断异步可变值是否已完成设置
    pub fn is_complete(&self) -> bool {
        self
            .0
            .status
            .load(Ordering::Acquire) & 4 != 0
    }

    /// 锁住待修改的异步可变值，并返回当前异步可变值的守护者，如果异步可变值已完成修改则返回空
    pub fn lock(&self) -> Option<AsyncVariableGuard<V>> {
        let mut spin_len = 1;
        loop {
            match self
                .0
                .status
                .compare_exchange(1,
                                  3,
                                  Ordering::Acquire,
                                  Ordering::Relaxed) {
                Err(0) => {
                    //异步可变值还未就绪，则自旋等待
                    spin_len = spin(spin_len);
                },
                Err(3) => {
                    //已锁但未获取到锁，则自旋等待
                    spin_len = spin(spin_len);
                },
                Err(_) => {
                    //已完成，则返回空
                    return None;
                }
                Ok(_) => {
                    //已锁且获取到锁，则返回异步可变值的守护者
                    let guard = AsyncVariableGuard {
                        value: &self.0.value,
                        waker: &self.0.waker,
                        status: &self.0.status,
                    };

                    return Some(guard)
                },
            }
        }
    }
}

// 内部异步可变值，在完成前允许被修改多次
pub struct InnerAsyncVariable<V: Send + 'static> {
    value:  UnsafeCell<Option<V>>,      //值
    waker:  UnsafeCell<Option<Waker>>,  //唤醒器
    status: AtomicU8,                   //状态
}

///
/// 等待异步任务运行的结果
///
pub struct AsyncWaitResult<V: Send + 'static>(pub Arc<RefCell<Option<Result<V>>>>);

unsafe impl<V: Send + 'static> Send for AsyncWaitResult<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncWaitResult<V> {}

impl<V: Send + 'static> Clone for AsyncWaitResult<V> {
    fn clone(&self) -> Self {
        AsyncWaitResult(self.0.clone())
    }
}

///
/// 等待异步任务运行的结果集
///
pub struct AsyncWaitResults<V: Send + 'static>(pub Arc<RefCell<Option<Vec<Result<V>>>>>);

unsafe impl<V: Send + 'static> Send for AsyncWaitResults<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncWaitResults<V> {}

impl<V: Send + 'static> Clone for AsyncWaitResults<V> {
    fn clone(&self) -> Self {
        AsyncWaitResults(self.0.clone())
    }
}

///
/// 异步定时器任务
///
pub enum AsyncTimingTask<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static = (),
> {
    Pended(TaskId),                 //已挂起的定时任务
    WaitRun(Arc<AsyncTask<P, O>>),  //等待执行的定时任务
}

///
/// 异步任务本地定时器
///
pub struct AsyncTaskTimer<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static = (),
> {
    producor:   Sender<(usize, AsyncTimingTask<P, O>)>,                     //定时任务生产者
    consumer:   Receiver<(usize, AsyncTimingTask<P, O>)>,                   //定时任务消费者
    timer:      Arc<RefCell<Timer<AsyncTimingTask<P, O>, 1000, 60, 3>>>,    //定时器
    now:        Instant,                                                    //当前时间
}

unsafe impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Send for AsyncTaskTimer<P, O> {}
unsafe impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Sync for AsyncTaskTimer<P, O> {}

impl<
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> AsyncTaskTimer<P, O> {
    /// 构建异步任务本地定时器
    pub fn new() -> Self {
        let (producor, consumer) = unbounded();
        let now = Instant::now();

        AsyncTaskTimer {
            producor,
            consumer,
            timer: Arc::new(RefCell::new(Timer::<AsyncTimingTask<P, O>, 1000, 60, 3>::default())),
            now,
        }
    }

    /// 获取定时任务生产者
    #[inline]
    pub fn get_producor(&self) -> &Sender<(usize, AsyncTimingTask<P, O>)> {
        &self.producor
    }

    /// 获取剩余未到期的定时器任务数量
    #[inline]
    pub fn len(&self) -> usize {
        let timer = self.timer.as_ref().borrow();
        timer.add_count() - timer.remove_count()
    }

    /// 设置定时器
    pub fn set_timer(&self, task: AsyncTimingTask<P, O>, timeout: usize) -> usize {
        self
            .timer
            .borrow_mut()
            .push(timeout, task)
            .data()
            .as_ffi() as usize
    }

    /// 取消定时器
    pub fn cancel_timer(&self, timer_ref: usize) -> Option<AsyncTimingTask<P, O>> {
        if let Some(item) = self
            .timer
            .borrow_mut()
            .cancel(KeyData::from_ffi(timer_ref as u64).into()) {
            Some(item)
        } else {
            None
        }
    }

    /// 消费所有定时任务，返回定时任务数量
    pub fn consume(&self) -> usize {
        let mut len = 0;
        let timer_tasks = self.consumer.try_iter().collect::<Vec<(usize, AsyncTimingTask<P, O>)>>();
        for (timeout, task) in timer_tasks {
            self.set_timer(task, timeout);
            len += 1;
        }

        len
    }

    /// 判断当前时间是否有可以弹出的任务，如果有可以弹出的任务，则返回当前时间，否则返回空
    pub fn is_require_pop(&self) -> Option<u64> {
        let current_time = self.now.elapsed().as_millis() as u64;
        if self.timer.borrow_mut().is_ok(current_time) {
            Some(current_time)
        } else {
            None
        }
    }

    /// 从定时器中弹出指定时间的一个到期任务
    pub fn pop(&self, current_time: u64) -> Option<(usize, AsyncTimingTask<P, O>)> {
        if let Some((key, item)) = self.timer.borrow_mut().pop_kv(current_time) {
            Some((key.data().as_ffi() as usize, item))
        } else {
            None
        }
    }
}

///
/// 等待指定超时
///
pub struct AsyncWaitTimeout<
    RT: AsyncRuntime<O>,
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static = (),
> {
    rt:         RT,                                     //当前运行时
    producor:   Sender<(usize, AsyncTimingTask<P, O>)>, //超时请求生产者
    timeout:    usize,                                  //超时时长，单位ms
    expired:    AtomicBool,                             //是否已过期
}

unsafe impl<
    RT: AsyncRuntime<O>,
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Send for AsyncWaitTimeout<RT, P, O> {}
unsafe impl<
    RT: AsyncRuntime<O>,
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Sync for AsyncWaitTimeout<RT, P, O> {}

impl<
    RT: AsyncRuntime<O>,
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> Future for AsyncWaitTimeout<RT, P, O> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if (&self).expired.load(Ordering::Relaxed) {
            //已到期，则返回
            return Poll::Ready(());
        } else {
            //未到期，则设置为已到期
            (&self).expired.store(true, Ordering::Relaxed);
        }

        let task_id = self.rt.alloc();
        let reply = self.rt.pending(&task_id, cx.waker().clone());

        //发送超时请求，并返回
        (&self).producor.send(((&self).timeout, AsyncTimingTask::Pended(task_id)));
        reply
    }
}

impl<
    RT: AsyncRuntime<O>,
    P: AsyncTaskPoolExt<O> + AsyncTaskPool<O>,
    O: Default + 'static,
> AsyncWaitTimeout<RT, P, O> {
    /// 构建等待指定超时任务的方法
    pub fn new(rt: RT,
               producor: Sender<(usize, AsyncTimingTask<P, O>)>,
               timeout: usize) -> Self {
        AsyncWaitTimeout {
            rt,
            producor,
            timeout,
            expired: AtomicBool::new(false), //设置初始值
        }
    }
}

///
/// 等待异步任务执行完成
///
pub struct AsyncWait<V: Send + 'static>(AsyncWaitAny<V>);

unsafe impl<V: Send + 'static> Send for AsyncWait<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncWait<V> {}

/*
* 等待异步任务执行完成同步方法
*/
impl<V: Send + 'static> AsyncWait<V> {
    /// 派发指定超时时间的指定任务到指定的运行时，并返回派发是否成功
    pub fn spawn<RT, O, F>(&self,
                           rt: RT,
                           timeout: Option<usize>,
                           future: F) -> Result<()>
        where RT: AsyncRuntime<O>,
              O: Default + 'static,
              F: Future<Output = Result<V>> + Send + 'static {
        self.0.spawn(rt.clone(), future)?;

        if let Some(timeout) = timeout {
            //设置了超时时间
            let rt_copy = rt.clone();
            self.0.spawn(rt, async move {
                rt_copy.timeout(timeout).await;

                //返回超时错误
                Err(Error::new(ErrorKind::TimedOut, format!("Time out")))
            })
        } else {
            //未设置超时时间
            Ok(())
        }
    }

    /// 派发指定超时时间的指定任务到本地运行时，并返回派发是否成功
    pub fn spawn_local<O, F>(&self,
                             timeout: Option<usize>,
                             future: F) -> Result<()>
        where O: Default + 'static,
              F: Future<Output = Result<V>> + Send + 'static {
        if let Some(rt) = local_async_runtime::<O>() {
            //当前线程有绑定运行时
            self.0.spawn_local(future)?;

            if let Some(timeout) = timeout {
                //设置了超时时间
                let rt_copy = rt.clone();
                self.0.spawn_local(async move {
                    rt_copy.timeout(timeout).await;

                    //返回超时错误
                    Err(Error::new(ErrorKind::TimedOut, format!("Time out")))
                })
            } else {
                //未设置超时时间
                Ok(())
            }
        } else {
            //当前线程未绑定运行时
            Err(Error::new(ErrorKind::Other, format!("Spawn wait task failed, reason: local async runtime not exist")))
        }
    }
}

/*
* 等待异步任务执行完成异步方法
*/
impl<V: Send + 'static> AsyncWait<V> {
    /// 异步等待已派发任务的结果
    pub async fn wait_result(self) -> Result<V> {
        self.0.wait_result().await
    }
}

///
/// 等待任意异步任务执行完成
///
pub struct AsyncWaitAny<V: Send + 'static> {
    capacity:       usize,                      //派发任务的容量
    producor:       AsyncSender<Result<V>>,     //异步返回值生成器
    consumer:       AsyncReceiver<Result<V>>,   //异步返回值接收器
}

unsafe impl<V: Send + 'static> Send for AsyncWaitAny<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncWaitAny<V> {}

/*
* 等待任意异步任务执行完成同步方法
*/
impl<V: Send + 'static> AsyncWaitAny<V> {
    /// 派发指定任务到指定的运行时，并返回派发是否成功
    pub fn spawn<RT, O, F>(&self,
                           rt: RT,
                           future: F) -> Result<()>
        where RT: AsyncRuntime<O>,
              O: Default + 'static,
              F: Future<Output = Result<V>> + Send + 'static {
        let producor = self.producor.clone();
        rt.spawn(rt.alloc(), async move {
            let value = future.await;
            producor.into_send_async(value).await;

            //返回异步任务的默认值
            Default::default()
        })
    }

    /// 派发指定任务到本地运行时，并返回派发是否成功
    pub fn spawn_local<F>(&self,
                          future: F) -> Result<()>
        where F: Future<Output = Result<V>> + Send + 'static {
        if let Some(rt) = local_async_runtime() {
            //本地线程有绑定运行时
            let producor = self.producor.clone();
            rt.spawn(async move {
                let value = future.await;
                producor.into_send_async(value).await;
            })
        } else {
            //本地线程未绑定运行时
            Err(Error::new(ErrorKind::Other, format!("Spawn wait any task failed, reason: local async runtime not exist")))
        }
    }
}

/*
* 等待任意异步任务执行完成异步方法
*/
impl<V: Send + 'static> AsyncWaitAny<V> {
    /// 异步等待任意已派发任务的结果
    pub async fn wait_result(self) -> Result<V> {
        match self.consumer.recv_async().await {
            Err(e) => {
                //接收错误，则立即返回
                Err(Error::new(ErrorKind::Other, format!("Wait any result failed, reason: {:?}", e)))
            },
            Ok(result) => {
                //接收成功，则立即返回
                result
            },
        }
    }
}

///
/// 等待任意异步任务执行完成
///
pub struct AsyncWaitAnyCallback<V: Send + 'static> {
    capacity:   usize,                      //派发任务的容量
    producor:   AsyncSender<Result<V>>,     //异步返回值生成器
    consumer:   AsyncReceiver<Result<V>>,   //异步返回值接收器
}

unsafe impl<V: Send + 'static> Send for AsyncWaitAnyCallback<V> {}
unsafe impl<V: Send + 'static> Sync for AsyncWaitAnyCallback<V> {}

/*
* 等待任意异步任务执行完成同步方法
*/
impl<V: Send + 'static> AsyncWaitAnyCallback<V> {
    /// 派发指定任务到指定的运行时，并返回派发是否成功
    pub fn spawn<RT, O, F>(&self,
                           rt: RT,
                           future: F) -> Result<()>
        where RT: AsyncRuntime<O>,
              O: Default + 'static,
              F: Future<Output = Result<V>> + Send + 'static {
        let producor = self.producor.clone();
        rt.spawn(rt.alloc(), async move {
            let value = future.await;
            producor.into_send_async(value).await;

            //返回异步任务的默认值
            Default::default()
        })
    }

    /// 派发指定任务到本地运行时，并返回派发是否成功
    pub fn spawn_local<F>(&self,
                          future: F) -> Result<()>
        where F: Future<Output = Result<V>> + Send + 'static {
        if let Some(rt) = local_async_runtime() {
            //当前线程有绑定运行时
            let producor = self.producor.clone();
            rt.spawn(async move {
                let value = future.await;
                producor.into_send_async(value).await;
            })
        } else {
            //当前线程未绑定运行时
            Err(Error::new(ErrorKind::Other, format!("Spawn wait any task failed by callback, reason: current async runtime not exist")))
        }
    }
}

/*
* 等待任意异步任务执行完成异步方法
*/
impl<V: Send + 'static> AsyncWaitAnyCallback<V> {
    /// 异步等待满足用户回调需求的已派发任务的结果
    pub async fn wait_result(mut self,
                             callback: impl Fn(&Result<V>) -> bool + Send + Sync + 'static) -> Result<V> {
        let checker = create_checker(self.capacity, callback);
        loop {
            match self.consumer.recv_async().await {
                Err(e) => {
                    //接收错误，则立即返回
                    return Err(Error::new(ErrorKind::Other, format!("Wait any result failed by callback, reason: {:?}", e)));
                },
                Ok(result) => {
                    //接收成功，则检查是否立即返回
                    if checker(&result) {
                        //检查通过，则立即唤醒等待的任务，否则等待其它任务唤醒
                        return result;
                    }
                },
            }
        }
    }
}

// 根据用户提供的回调，生成检查器
fn create_checker<V, F>(len: usize,
                        callback: F) -> Arc<dyn Fn(&Result<V>) -> bool + Send + Sync + 'static>
    where V: Send + 'static,
          F: Fn(&Result<V>) -> bool + Send + Sync + 'static {
    let mut check_counter = AtomicUsize::new(len); //初始化检查计数器
    Arc::new(move |result| {
        if check_counter.fetch_sub(1, Ordering::SeqCst) == 1 {
            //最后一个任务的检查，则忽略用户回调，并立即返回成功
            true
        } else {
            //不是最后一个任务的检查，则调用用户回调，并根据用户回调确定是否成功
            callback(result)
        }
    })
}

///
/// 异步映射归并
///
pub struct AsyncMapReduce<V: Send + 'static> {
    count:          usize,                              //派发的任务数量
    capacity:       usize,                              //派发任务的容量
    producor:       AsyncSender<(usize, Result<V>)>,    //异步返回值生成器
    consumer:       AsyncReceiver<(usize, Result<V>)>,  //异步返回值接收器
}

unsafe impl<V: Send + 'static> Send for AsyncMapReduce<V> {}

/*
* 异步映射归并同步方法
*/
impl<V: Send + 'static> AsyncMapReduce<V> {
    /// 映射指定任务到指定的运行时，并返回任务序号
    pub fn map<RT, O, F>(&mut self, rt: RT, future: F) -> Result<usize>
        where RT: AsyncRuntime<O>,
              O: Default + 'static,
              F: Future<Output = Result<V>> + Send + 'static {
        if self.count >= self.capacity {
            //已派发任务已达可派发任务的限制，则返回错误
            return Err(Error::new(ErrorKind::Other, format!("Map task to runtime failed, capacity: {}, reason: out of capacity", self.capacity)));
        }

        let index = self.count;
        let producor = self.producor.clone();
        rt.spawn(rt.alloc(), async move {
            let value = future.await;
            producor.into_send_async((index, value)).await;

            //返回异步任务的默认值
            Default::default()
        })?;

        self.count += 1; //派发任务成功，则计数
        Ok(index)
    }
}

/*
* 异步映射归并异步方法
*/
impl<V: Send + 'static> AsyncMapReduce<V> {
    /// 归并所有派发的任务
    pub async fn reduce(self, order: bool) -> Result<Vec<Result<V>>> {
        let mut count = self.count;
        let mut results = Vec::with_capacity(count);
        while count > 0 {
            match self.consumer.recv_async().await {
                Err(e) => {
                    //接收错误，则立即返回
                    return Err(Error::new(ErrorKind::Other, format!("Reduce result failed, reason: {:?}", e)));
                },
                Ok((index, result)) => {
                    //接收成功，则继续
                    results.push((index, result));
                    count -= 1;
                },
            }
        }

        if order {
            //需要对结果集进行排序
            results.sort_by_key(|(key, _value)| {
                key.clone()
            });
        }
        let (_, values) = results
            .into_iter()
            .unzip::<usize, Result<V>, Vec<usize>, Vec<Result<V>>>();

        Ok(values)
    }
}

///
/// 异步管道过滤器结果
///
pub enum AsyncPipelineResult<O: 'static> {
    Disconnect,     //关闭管道
    Filtered(O),    //过滤后的值
}

///
/// 派发一个工作线程
/// 返回线程的句柄，可以通过句柄关闭线程
/// 线程在没有任务可以执行时会休眠，当派发任务或唤醒任务时会自动唤醒线程
///
pub fn spawn_worker_thread<F0, F1>(thread_name: &str,
                                   thread_stack_size: usize,
                                   thread_handler: Arc<AtomicBool>,
                                   thread_waker: Arc<(AtomicBool, Mutex<()>, Condvar)>, //用于唤醒运行时所在线程的条件变量
                                   sleep_timeout: u64,                                  //休眠超时时长，单位毫秒
                                   loop_interval: Option<u64>,                          //工作者线程循环的间隔时长，None为无间隔，单位毫秒
                                   loop_func: F0,
                                   get_queue_len: F1) -> Arc<AtomicBool>
    where F0: Fn() -> (bool, Duration) + Send + 'static,
          F1: Fn() -> usize + Send + 'static {
    let thread_status_copy = thread_handler.clone();

    thread::Builder::new()
        .name(thread_name.to_string())
        .stack_size(thread_stack_size).spawn(move || {
        let mut sleep_count = 0;

        while thread_handler.load(Ordering::Relaxed) {
            let (is_no_task, run_time) = loop_func();

            if is_no_task {
                //当前没有任务
                if sleep_count > 1 {
                    //当前没有任务连续达到2次，则休眠线程
                    sleep_count = 0; //重置休眠计数
                    let (is_sleep, lock, condvar) = &*thread_waker;
                    let mut locked = lock.lock();
                    if get_queue_len() > 0 {
                        //当前有任务，则继续工作
                        continue;
                    }

                    if !is_sleep.load(Ordering::Relaxed) {
                        //如果当前未休眠，则休眠
                        is_sleep.store(true, Ordering::SeqCst);
                        if condvar
                            .wait_for(
                                &mut locked,
                                Duration::from_millis(sleep_timeout),
                            )
                            .timed_out()
                        {
                            //条件超时唤醒，则设置状态为未休眠
                            is_sleep.store(false, Ordering::SeqCst);
                        }
                    }

                    continue; //唤醒后立即尝试执行任务
                }

                sleep_count += 1; //休眠计数
                if let Some(interval) = &loop_interval {
                    //设置了循环间隔时长
                    if let Some(remaining_interval) = Duration::from_millis(*interval).checked_sub(run_time){
                        //本次运行少于循环间隔，则休眠剩余的循环间隔，并继续执行任务
                        thread::sleep(remaining_interval);
                    }
                }
            } else {
                //当前有任务
                sleep_count = 0; //重置休眠计数
                if let Some(interval) = &loop_interval {
                    //设置了循环间隔时长
                    if let Some(remaining_interval) = Duration::from_millis(*interval).checked_sub(run_time){
                        //本次运行少于循环间隔，则休眠剩余的循环间隔，并继续执行任务
                        thread::sleep(remaining_interval);
                    }
                }
            }
        }
    });

    thread_status_copy
}

/// 唤醒工作者所在线程，如果线程当前正在运行，则忽略
pub fn wakeup_worker_thread<O: Default + 'static, P: AsyncTaskPoolExt<O> + AsyncTaskPool<O, Pool = P>>(worker_waker: &Arc<(AtomicBool, Mutex<()>, Condvar)>, rt: &SingleTaskRuntime<O, P>) {
    //检查工作者所在线程是否需要唤醒
    if worker_waker.0.load(Ordering::Relaxed) && rt.len() > 0 {
        let (is_sleep, lock, condvar) = &**worker_waker;
        let locked = lock.lock();
        is_sleep.store(false, Ordering::SeqCst); //设置为未休眠
        let _ = condvar.notify_one();
    }
}

/// 注册全局异常处理器，会替换当前全局异常处理器
pub fn register_global_panic_handler<Handler>(handler: Handler)
    where Handler: Fn(thread::Thread, String, Option<String>, Option<(String, u32, u32)>) -> Option<i32> + Send + Sync + 'static {
    set_hook(Box::new(move |panic_info| {
        let thread_info = thread::current();

        let payload = panic_info.payload();
        let payload_info = match payload.downcast_ref::<&str>() {
            None => {
                //不是String
                match payload.downcast_ref::<String>() {
                    None => {
                        //不是&'static str，则返回未知异常
                        "Unknow panic".to_string()
                    },
                    Some(info) => {
                        info.clone()
                    }
                }
            },
            Some(info) => {
                info.to_string()
            }
        };

        let other_info = if let Some(arg) = panic_info.message() {
            if let Some(s) = arg.as_str() {
                Some(s.to_string())
            } else {
                None
            }
        } else {
            None
        };

        let location = if let Some(location) = panic_info.location() {
            Some((location.file().to_string(), location.line(), location.column()))
        } else {
            None
        };

        if let Some(exit_code) = handler(thread_info, payload_info, other_info, location) {
            //需要关闭当前进程
            std::process::exit(exit_code);
        }
    }));
}

/// 替换全局内存分配错误处理器
pub fn replace_global_alloc_error_handler() {
    set_alloc_error_hook(global_alloc_error_handle);
}

fn global_alloc_error_handle(layout: Layout) {
    let bt = Backtrace::new();
    eprintln!("[UTC: {}][Thread: {}]Global memory allocation of {:?} bytes failed, stacktrace: \n{:?}",
              SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis(),
              thread::current().name().unwrap_or(""),
              layout.size(),
              bt);
}

///单调递增时间
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MontonicTime {
    Normal(std::time::Instant),    //标准单调递增时间
    Fast((i64, i64)),   //快速单调递增时间，只支持类Linux系统
}

unsafe impl Send for MontonicTime {}
unsafe impl Sync for MontonicTime {}

impl MontonicTime {
    /// 构建一个单调递增时间
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn now() -> Self {
        MontonicTime::Normal(std::time::Instant::now())
    }

    /// 构建一个单调递增时间
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn now() -> Self {
        MontonicTime::Fast(now_monotonic())
    }

    /// 获取从构建单调递增时间开始已过去的时间
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn elapsed(&self) -> Duration {
        if let MontonicTime::Normal(now) = self {
            now.elapsed()
        } else {
            panic!("Take elapsed failed, reason: invalid target os");
        }
    }

    /// 获取从构建单调递增时间开始已过去的时间，精度为ms
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn elapsed(&self) -> Duration {
        if let MontonicTime::Fast((sec0, nsec0)) = self {
            let (sec1, nsec1) = now_monotonic();
            Duration::new((sec1 - sec0) as u64, (nsec1 - nsec0) as u32)
        } else {
            panic!("Take elapsed failed, reason: invalid target os");
        }
    }
}

/// 快速获取单调时间
#[inline]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn now_monotonic() -> (i64, i64) {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_COARSE, &mut time) };
    assert!(ret == 0);

    (time.tv_sec as i64, time.tv_nsec as i64)
}