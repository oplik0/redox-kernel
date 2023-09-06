//! # Schemes
//! A scheme is a primitive for handling filesystem syscalls in Redox.
//! Schemes accept paths from the kernel for `open`, and file descriptors that they generate
//! are then passed for operations like `close`, `read`, `write`, etc.
//!
//! The kernel validates paths and file descriptors before they are passed to schemes,
//! also stripping the scheme identifier of paths if necessary.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::ToString,
    sync::Arc,
    vec::Vec,
};
use syscall::{MunmapFlags, SendFdFlags, EventFlags, SEEK_SET, SEEK_CUR, SEEK_END};
use core::sync::atomic::AtomicUsize;
use spin::{Once, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::context::file::FileDescription;
use crate::context::{memory::AddrSpace, file::FileDescriptor};
use crate::syscall::error::*;
use crate::syscall::usercopy::{UserSliceRo, UserSliceWo};

#[cfg(all(feature = "acpi", any(target_arch = "x86", target_arch = "x86_64")))]
use self::acpi::AcpiScheme;
#[cfg(all(any(target_arch = "aarch64")))]
use self::dtb::DtbScheme;

use self::debug::DebugScheme;
use self::event::EventScheme;
use self::irq::IrqScheme;
use self::itimer::ITimerScheme;
use self::memory::MemoryScheme;
use self::pipe::PipeScheme;
use self::proc::ProcScheme;
use self::root::RootScheme;
use self::serio::SerioScheme;
use self::sys::SysScheme;
use self::time::TimeScheme;
use self::user::{UserInner, UserScheme};

/// When compiled with the "acpi" feature - `acpi:` - allows drivers to read a limited set of ACPI tables.
#[cfg(all(feature = "acpi", any(target_arch = "x86", target_arch = "x86_64")))]
pub mod acpi;
#[cfg(all(any(target_arch = "aarch64")))]
pub mod dtb;

/// `debug:` - provides access to serial console
pub mod debug;

/// `event:` - allows reading of `Event`s which are registered using `fevent`
pub mod event;

/// `irq:` - allows userspace handling of IRQs
pub mod irq;

/// `itimer:` - support for getitimer and setitimer
pub mod itimer;

/// `memory:` - a scheme for accessing physical memory
pub mod memory;

/// `pipe:` - used internally by the kernel to implement `pipe`
pub mod pipe;

/// `proc:` - allows tracing processes and reading/writing their memory
pub mod proc;

/// `:` - allows the creation of userspace schemes, tightly dependent on `user`
pub mod root;

/// `serio:` - provides access to ps/2 devices
pub mod serio;

/// `sys:` - system information, such as the context list and scheme list
pub mod sys;

/// `time:` - allows reading time, setting timeouts and getting events when they are met
pub mod time;

/// A wrapper around userspace schemes, tightly dependent on `root`
pub mod user;

/// Limit on number of schemes
pub const SCHEME_MAX_SCHEMES: usize = 65_536;

// Unique identifier for a scheme namespace.
int_like!(SchemeNamespace, AtomicSchemeNamespace, usize, AtomicUsize);

// Unique identifier for a scheme.
int_like!(SchemeId, usize);

// Unique identifier for a file descriptor.
int_like!(FileHandle, AtomicFileHandle, usize, AtomicUsize);

pub struct SchemeIter<'a> {
    inner: Option<::alloc::collections::btree_map::Iter<'a, Box<str>, SchemeId>>
}

impl<'a> Iterator for SchemeIter<'a> {
    type Item = (&'a Box<str>, &'a SchemeId);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.as_mut().and_then(|iter| iter.next())
    }
}

/// Scheme list type
pub struct SchemeList {
    map: BTreeMap<SchemeId, KernelSchemes>,
    names: BTreeMap<SchemeNamespace, BTreeMap<Box<str>, SchemeId>>,
    next_ns: usize,
    next_id: usize
}

impl SchemeList {
    /// Create a new scheme list.
    pub fn new() -> Self {
        let mut list = SchemeList {
            map: BTreeMap::new(),
            names: BTreeMap::new(),
            // Scheme namespaces always start at 1. 0 is a reserved namespace, the null namespace
            next_ns: 1,
            next_id: 1
        };
        list.new_null();
        list.new_root();
        list
    }

    /// Initialize the null namespace
    fn new_null(&mut self) {
        let ns = SchemeNamespace(0);
        self.names.insert(ns, BTreeMap::new());

        //TODO: Only memory: is in the null namespace right now. It should be removed when
        //anonymous mmap's are implemented
        self.insert(ns, "memory", |_| KernelSchemes::Memory).unwrap();
        self.insert(ns, "thisproc", |_| KernelSchemes::Proc(Arc::new(ProcScheme::restricted()))).unwrap();
        self.insert(ns, "pipe", |scheme_id| {
            PipeScheme::init(scheme_id);
            KernelSchemes::Pipe
        }).unwrap();
    }

    /// Initialize a new namespace
    fn new_ns(&mut self) -> SchemeNamespace {
        let ns = SchemeNamespace(self.next_ns);
        self.next_ns += 1;
        self.names.insert(ns, BTreeMap::new());

        self.insert(ns, "", |scheme_id| KernelSchemes::Root(Arc::new(RootScheme::new(ns, scheme_id)))).unwrap();
        self.insert(ns, "event", |_| KernelSchemes::Event).unwrap();
        self.insert(ns, "itimer", |_| KernelSchemes::ITimer(Arc::new(ITimerScheme::new()))).unwrap();
        self.insert(ns, "memory", |_| KernelSchemes::Memory).unwrap();
        self.insert(ns, "pipe", |scheme_id| {
            PipeScheme::init(scheme_id);
            KernelSchemes::Pipe
        }).unwrap();
        self.insert(ns, "sys", |_| KernelSchemes::Sys(Arc::new(SysScheme::new()))).unwrap();
        self.insert(ns, "time", |scheme_id| KernelSchemes::Time(Arc::new(TimeScheme::new(scheme_id)))).unwrap();

        ns
    }

    /// Initialize the root namespace
    fn new_root(&mut self) {
        // Do common namespace initialization
        let ns = self.new_ns();

        // These schemes should only be available on the root
        #[cfg(all(feature = "acpi", any(target_arch = "x86", target_arch = "x86_64")))]
        self.insert(ns, "kernel.acpi", |scheme_id| {
            AcpiScheme::init(scheme_id);
            KernelSchemes::Acpi
        }).unwrap();

        #[cfg(all(any(target_arch = "aarch64")))]
        {
            self.insert(ns, "kernel.dtb", |scheme_id| Arc::new(DtbScheme::new(scheme_id))).unwrap();
        }

        self.insert(ns, "debug", |scheme_id| {
            DebugScheme::init(scheme_id);
            KernelSchemes::Debug
        }).unwrap();
        self.insert(ns, "irq", |scheme_id| KernelSchemes::Irq(Arc::new(IrqScheme::new(scheme_id)))).unwrap();
        self.insert(ns, "proc", |scheme_id| KernelSchemes::Proc(Arc::new(ProcScheme::new(scheme_id)))).unwrap();
        self.insert(ns, "thisproc", |_| KernelSchemes::Proc(Arc::new(ProcScheme::restricted()))).unwrap();
        self.insert(ns, "serio", |scheme_id| {
            SerioScheme::init(scheme_id);
            KernelSchemes::Serio
        }).unwrap();
    }

    pub fn make_ns(&mut self, from: SchemeNamespace, names: impl IntoIterator<Item = Box<str>>) -> Result<SchemeNamespace> {
        // Create an empty namespace
        let to = self.new_ns();

        // Copy requested scheme IDs
        for name in names {
            let Some((id, _scheme)) = self.get_name(from, &name) else {
                return Err(Error::new(ENODEV));
            };

            if let Some(ref mut names) = self.names.get_mut(&to) {
                if names.insert(name.to_string().into_boxed_str(), id).is_some() {
                    return Err(Error::new(EEXIST));
                }
            } else {
                panic!("scheme namespace not found");
            }
        }

        Ok(to)
    }

    pub fn iter_name(&self, ns: SchemeNamespace) -> SchemeIter {
        SchemeIter {
            inner: self.names.get(&ns).map(|names| names.iter())
        }
    }

    /// Get the nth scheme.
    pub fn get(&self, id: SchemeId) -> Option<&KernelSchemes> {
        self.map.get(&id)
    }

    pub fn get_name(&self, ns: SchemeNamespace, name: &str) -> Option<(SchemeId, &KernelSchemes)> {
        if let Some(names) = self.names.get(&ns) {
            if let Some(&id) = names.get(name) {
                return self.get(id).map(|scheme| (id, scheme));
            }
        }
        None
    }

    /// Create a new scheme.
    pub fn insert(&mut self, ns: SchemeNamespace, name: &str, scheme_fn: impl FnOnce(SchemeId) -> KernelSchemes) -> Result<SchemeId> {
        self.insert_and_pass(ns, name, |id| (scheme_fn(id), ())).map(|(id, ())| id)
    }

    pub fn insert_and_pass<T>(&mut self, ns: SchemeNamespace, name: &str, scheme_fn: impl FnOnce(SchemeId) -> (KernelSchemes, T)) -> Result<(SchemeId, T)> {
        if let Some(names) = self.names.get(&ns) {
            if names.contains_key(name) {
                return Err(Error::new(EEXIST));
            }
        }

        if self.next_id >= SCHEME_MAX_SCHEMES {
            self.next_id = 1;
        }

        while self.map.contains_key(&SchemeId(self.next_id)) {
            self.next_id += 1;
        }

        /* Allow scheme list to grow if required
        if self.next_id >= SCHEME_MAX_SCHEMES {
            return Err(Error::new(EAGAIN));
        }
        */

        let id = SchemeId(self.next_id);
        self.next_id += 1;

        let (new_scheme, t) = scheme_fn(id);

        assert!(self.map.insert(id, new_scheme).is_none());
        if let Some(ref mut names) = self.names.get_mut(&ns) {
            assert!(names.insert(name.to_string().into_boxed_str(), id).is_none());
        } else {
            // Nonexistent namespace, posssibly null namespace
            return Err(Error::new(ENODEV));
        }
        Ok((id, t))
    }

    /// Remove a scheme
    pub fn remove(&mut self, id: SchemeId) {
        assert!(self.map.remove(&id).is_some());
        for (_ns, names) in self.names.iter_mut() {
            let mut remove = Vec::with_capacity(1);
            for (name, name_id) in names.iter() {
                if name_id == &id {
                    remove.push(name.clone());
                }
            }
            for name in remove {
                assert!(names.remove(&name).is_some());
            }
        }
    }
}

/// Schemes list
static SCHEMES: Once<RwLock<SchemeList>> = Once::new();

/// Initialize schemes, called if needed
fn init_schemes() -> RwLock<SchemeList> {
    RwLock::new(SchemeList::new())
}

/// Get the global schemes list, const
pub fn schemes() -> RwLockReadGuard<'static, SchemeList> {
    SCHEMES.call_once(init_schemes).read()
}

/// Get the global schemes list, mutable
pub fn schemes_mut() -> RwLockWriteGuard<'static, SchemeList> {
    SCHEMES.call_once(init_schemes).write()
}

#[allow(unused_variables)]
pub trait KernelScheme: Send + Sync + 'static {
    fn kopen(&self, path: &str, flags: usize, _ctx: CallerCtx) -> Result<OpenResult> {
        Err(Error::new(ENOENT))
    }

    fn as_filetable(&self, number: usize) -> Result<Arc<RwLock<Vec<Option<FileDescriptor>>>>> {
        Err(Error::new(EBADF))
    }
    fn as_addrspace(&self, number: usize) -> Result<Arc<RwLock<AddrSpace>>> {
        Err(Error::new(EBADF))
    }
    fn as_sigactions(&self, number: usize) -> Result<Arc<RwLock<Vec<(crate::syscall::data::SigAction, usize)>>>> {
        Err(Error::new(EBADF))
    }

    fn kfmap(&self, number: usize, addr_space: &Arc<RwLock<AddrSpace>>, map: &crate::syscall::data::Map, consume: bool) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }
    fn kfunmap(&self, number: usize, offset: usize, size: usize, flags: MunmapFlags) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn kdup(&self, old_id: usize, buf: UserSliceRo, _caller: CallerCtx) -> Result<OpenResult> {
        Err(Error::new(EOPNOTSUPP))
    }
    fn kwrite(&self, id: usize, buf: UserSliceRo) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn kread(&self, id: usize, buf: UserSliceWo) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn kfpath(&self, id: usize, buf: UserSliceWo) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn kfutimens(&self, id: usize, buf: UserSliceRo) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn kfstat(&self, id: usize, buf: UserSliceWo) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn kfstatvfs(&self, id: usize, buf: UserSliceWo) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn ksendfd(&self, id: usize, desc: Arc<RwLock<FileDescription>>, flags: SendFdFlags, arg: u64) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fsync(&self, id: usize) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn ftruncate(&self, id: usize, len: usize) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn seek(&self, id: usize, pos: isize, whence: usize) -> Result<usize> {
        Err(Error::new(ESPIPE))
    }
    fn fchmod(&self, id: usize, new_mode: u16) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn fchown(&self, id: usize, new_uid: u32, new_gid: u32) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn fevent(&self, id: usize, flags: EventFlags) -> Result<EventFlags> {
        Err(Error::new(EBADF))
    }
    fn frename(&self, id: usize, new_path: &str, caller_ctx: CallerCtx) -> Result<()> {
        Err(Error::new(EBADF))
    }
    fn fcntl(&self, id: usize, cmd: usize, arg: usize) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn rmdir(&self, path: &str, ctx: CallerCtx) -> Result<()> {
        Err(Error::new(ENOENT))
    }
    fn unlink(&self, path: &str, ctx: CallerCtx) -> Result<()> {
        Err(Error::new(ENOENT))
    }
    fn close(&self, id: usize) -> Result<()> {
        Err(Error::new(EBADF))
    }

    // TODO: This demonstrates why we need to transition away from a dyn trait.
    fn as_user_inner(&self) -> Option<Result<Arc<UserInner>>> { None }
}

#[derive(Debug)]
pub enum OpenResult {
    SchemeLocal(usize),
    External(Arc<RwLock<FileDescription>>),
}
pub struct CallerCtx {
    pub pid: usize,
    pub uid: u32,
    pub gid: u32,
}

pub fn calc_seek_offset(cur_pos: usize, rel_pos: isize, whence: usize, len: usize) -> Result<usize> {
    match whence {
        SEEK_SET => usize::try_from(rel_pos).map_err(|_| Error::new(EINVAL)),
        SEEK_CUR => cur_pos.checked_add_signed(rel_pos).ok_or(Error::new(EOVERFLOW)),
        SEEK_END => len.checked_add_signed(rel_pos).ok_or(Error::new(EOVERFLOW)),

        _ => return Err(Error::new(EINVAL)),
    }
}

#[derive(Clone)]
pub enum KernelSchemes {
    Debug,
    Event,
    Irq(Arc<IrqScheme>),
    ITimer(Arc<ITimerScheme>),
    Memory,
    Pipe,
    Proc(Arc<ProcScheme>),
    Root(Arc<RootScheme>),
    Serio,
    Sys(Arc<SysScheme>),
    Time(Arc<TimeScheme>),
    User(UserScheme),

    #[cfg(all(feature = "acpi", any(target_arch = "x86", target_arch = "x86_64")))]
    Acpi,
}

impl core::ops::Deref for KernelSchemes {
    type Target = dyn KernelScheme;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Debug => &DebugScheme,
            Self::Event => &EventScheme,
            Self::Irq(scheme) => &**scheme,
            Self::ITimer(scheme) => &**scheme,
            Self::Memory => &MemoryScheme,
            Self::Pipe => &PipeScheme,
            Self::Proc(scheme) => &**scheme,
            Self::Root(scheme) => &**scheme,
            Self::Serio => &SerioScheme,
            Self::Sys(scheme) => &**scheme,
            Self::Time(scheme) => &**scheme,
            Self::User(scheme) => scheme,

            #[cfg(all(feature = "acpi", any(target_arch = "x86", target_arch = "x86_64")))]
            Self::Acpi => &AcpiScheme,
        }
    }
}
