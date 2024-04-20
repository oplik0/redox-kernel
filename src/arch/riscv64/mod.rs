#[macro_use]
pub mod macros;

/// Constants like memory locations
pub mod consts;

/// Debugging support
pub mod debug;

/// Devices
pub mod device;

/// Interrupt instructions
#[macro_use]
pub mod interrupt;

/// Inter-processor interrupts
pub mod ipi;

/// Paging
pub mod paging;

pub mod rmm;

/// Initialization and start function
pub mod start;

/// Early init support
pub mod init;
