//! Linux Landlock enforcement for child processes. The ruleset is built in the
//! parent before fork; the child only calls `prctl` and `landlock_restrict_self`
//! between fork and exec, which are async-signal-safe and allocate nothing.
//! Unsupported kernels yield `None` so callers can report that explicitly.

use std::{io, path::Path, path::PathBuf};

// Syscall numbers are shared by x86_64 and aarch64 (asm-generic table).
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: u32 = 1;

// Filesystem access rights (ABI 1).
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
// Remaining ABI-1 rights (write, remove, make-dir/reg/sym) are covered by
/// `FS_ALL`; read-write grants everything except device and socket creation.
/// Every ABI-1 filesystem right; handling all of them denies the rest.
const FS_ALL: u64 = (1 << 13) - 1;
const FS_READ_ONLY: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
const FS_READ_WRITE: u64 = FS_ALL & !(FS_MAKE_BLOCK | FS_MAKE_CHAR | FS_MAKE_SOCK | FS_MAKE_FIFO);

// Network access rights (ABI 4).
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// One classic-BPF instruction, as the kernel's `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// The kernel's `struct sock_fprog`; points at a filter the caller keeps alive.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Whether the running kernel enforces Landlock, and at which ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Enforced { abi: u32 },
    Unavailable,
}

pub fn status() -> Status {
    // SAFETY: a version query takes no attribute pointer.
    let abi = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < 1 {
        Status::Unavailable
    } else {
        Status::Enforced { abi: abi as u32 }
    }
}

/// A prepared, not-yet-applied restriction for one child process.
pub struct ChildSandbox {
    ruleset: i32,
    network_blocked: bool,
    /// Seccomp filter that refuses IPv4/IPv6 socket creation. It also covers UDP
    /// and DNS, which Landlock's TCP-only rights cannot restrict.
    socket_filter: Option<Box<[SockFilter]>>,
}

// SAFETY: the ruleset is an immutable kernel object identified by an fd that
// is only read (never closed) from the forking thread.
unsafe impl Send for ChildSandbox {}
unsafe impl Sync for ChildSandbox {}

impl ChildSandbox {
    /// Builds a ruleset that allows `writable` paths (read/write/create) and
    /// `readable` paths (read/execute). Unless `allow_network` is set, all TCP
    /// connect and bind are denied.
    /// Paths that do not exist are skipped. Returns `Ok(None)` when the kernel
    /// does not support Landlock.
    pub fn new(
        writable: &[PathBuf],
        readable: &[PathBuf],
        allow_network: bool,
    ) -> io::Result<Option<Self>> {
        let abi = match status() {
            Status::Enforced { abi } => abi,
            Status::Unavailable => return Ok(None),
        };
        let network_blocked = abi >= 4 && !allow_network;
        let attr = RulesetAttr {
            handled_access_fs: FS_ALL,
            handled_access_net: if network_blocked {
                NET_BIND_TCP | NET_CONNECT_TCP
            } else {
                0
            },
        };
        let size = if network_blocked {
            std::mem::size_of::<RulesetAttr>()
        } else {
            std::mem::size_of::<u64>()
        };
        // SAFETY: `attr` is a live, correctly laid out struct of `size` bytes
        // (a shorter size is valid for older ABIs, which omit net rights).
        let ruleset = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        } as i32;
        if ruleset < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket_filter = if allow_network {
            None
        } else {
            network_socket_filter()
        };
        let sandbox = ChildSandbox {
            ruleset,
            network_blocked,
            socket_filter,
        };
        for path in writable {
            sandbox.allow(path, FS_READ_WRITE)?;
        }
        for path in readable {
            sandbox.allow(path, FS_READ_ONLY)?;
        }
        Ok(Some(sandbox))
    }

    fn allow(&self, path: &Path, access: u64) -> io::Result<()> {
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            return Ok(());
        };
        // O_PATH needs no read permission on the target and works for files.
        // SAFETY: `c_path` is a valid NUL-terminated string for the call.
        let parent = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            };
        }
        // Directory-only rights (listing, creating, removing entries) are
        // rejected for regular files with EINVAL, so keep file-applicable ones.
        let is_dir = std::fs::metadata(path).is_ok_and(|meta| meta.is_dir());
        let access = if is_dir {
            access
        } else {
            access & (FS_EXECUTE | FS_READ_FILE | FS_WRITE_FILE)
        };
        let rule = PathBeneathAttr {
            allowed_access: access,
            parent_fd: parent,
        };
        // SAFETY: `rule` is a live packed struct matching the kernel layout.
        let added = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                self.ruleset,
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        let error = io::Error::last_os_error();
        // SAFETY: `parent` was opened above and is closed exactly once.
        unsafe { libc::close(parent) };
        if added < 0 {
            return Err(error);
        }
        Ok(())
    }

    /// Whether TCP connect/bind is denied by this ruleset.
    pub fn network_blocked(&self) -> bool {
        self.network_blocked
    }

    /// Applies the restriction to the calling process. Intended for
    /// `pre_exec`: performs only syscalls and allocates nothing.
    pub fn restrict_current(&self) -> io::Result<()> {
        // SAFETY: plain prctl/syscall invocations with scalar arguments.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, self.ruleset, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(filter) = &self.socket_filter {
                let program = SockFprog {
                    len: filter.len() as u16,
                    filter: filter.as_ptr(),
                };
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    SECCOMP_MODE_FILTER,
                    &program as *const SockFprog,
                    0,
                    0,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
}

impl Drop for ChildSandbox {
    fn drop(&mut self) {
        // SAFETY: the ruleset fd is owned by this value and closed once.
        unsafe { libc::close(self.ruleset) };
    }
}

// Seccomp constants (linux/seccomp.h and linux/filter.h).
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
// Offsets into `struct seccomp_data`: nr, arch, args[0] (low 32 bits).
const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
const SECCOMP_DATA_ARG0: u32 = 16;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// Refuses `socket(AF_INET|AF_INET6, ...)` with EPERM and kills the process if the
/// syscall ABI is not the one the filter was built for. Other families, including
/// AF_UNIX, are allowed. Returns `None` on architectures without a filter.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn network_socket_filter() -> Option<Box<[SockFilter]>> {
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 10;
    let jump = |jt: u8, jf: u8, k: u32| SockFilter {
        code: BPF_JMP_JEQ_K,
        jt,
        jf,
        k,
    };
    let load = |offset: u32| SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: offset,
    };
    let ret = |value: u32| SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: value,
    };
    // Jump offsets are relative to the next instruction.
    let program = vec![
        load(SECCOMP_DATA_ARCH),                     // 0
        jump(0, 7, AUDIT_ARCH),                      // 1: wrong ABI -> 9 (kill)
        load(SECCOMP_DATA_NR),                       // 2
        jump(0, 4, libc::SYS_socket as u32),         // 3: not socket -> 8 (allow)
        load(SECCOMP_DATA_ARG0),                     // 4
        jump(1, 0, AF_INET),                         // 5: AF_INET -> 7 (deny)
        jump(0, 1, AF_INET6),                        // 6: AF_INET6 -> 7, else 8
        ret(SECCOMP_RET_ERRNO | libc::EPERM as u32), // 7: deny
        ret(SECCOMP_RET_ALLOW),                      // 8
        ret(SECCOMP_RET_KILL_PROCESS),               // 9
    ];
    Some(program.into_boxed_slice())
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn network_socket_filter() -> Option<Box<[SockFilter]>> {
    None
}

/// Workspace confinement shared by commands and MCP servers.
///
/// Writable: `root`, the temp dir, `extra_writable`, and the package-manager
/// caches (`~/.cargo/{registry,git}`, `~/.npm`). `~/.cargo` as a whole is not
/// writable because it holds `credentials.toml`. Readable: system roots, PATH
/// entries, the rust toolchain and git configuration. Anything else in the home
/// directory, such as `~/.ssh`, is denied.
pub fn workspace_sandbox(
    root: &Path,
    extra_writable: &[PathBuf],
    allow_network: bool,
) -> io::Result<Option<ChildSandbox>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut writable = vec![root.to_path_buf(), std::env::temp_dir()];
    writable.extend(extra_writable.iter().cloned());
    let mut readable = system_read_roots();
    if let Some(path) = std::env::var_os("PATH") {
        readable.extend(std::env::split_paths(&path));
    }
    if let Some(home) = &home {
        writable.push(home.join(".cargo/registry"));
        writable.push(home.join(".cargo/git"));
        writable.push(home.join(".npm"));
        readable.push(home.join(".cargo/bin"));
        readable.push(home.join(".cargo/config.toml"));
        readable.push(home.join(".rustup"));
        readable.push(home.join(".gitconfig"));
        readable.push(home.join(".config/git"));
    }
    ChildSandbox::new(&writable, &readable, allow_network)
}

/// System locations a command normally needs to run: binaries, libraries,
/// configuration and the device/proc interfaces. Home directories and
/// credential stores are not included.
pub fn system_read_roots() -> Vec<PathBuf> {
    [
        "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/nix", "/proc",
        "/sys", "/dev",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_a_consistent_answer() {
        match status() {
            Status::Enforced { abi } => assert!(abi >= 1),
            Status::Unavailable => {}
        }
    }

    #[test]
    fn struct_layouts_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<RulesetAttr>(), 16);
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
        assert_eq!(std::mem::size_of::<SockFilter>(), 8);
        assert_eq!(std::mem::size_of::<SockFprog>(), 16);
    }

    /// Runs a Python probe under `sandbox` and returns its stdout lines.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn probe_under(sandbox: ChildSandbox, script: &str) -> Vec<String> {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("python3");
        command.args(["-c", script]);
        // SAFETY: restrict_current only issues prctl/syscall calls with scalar or
        // pre-built arguments, which is what a pre_exec hook may do.
        unsafe {
            command.pre_exec(move || sandbox.restrict_current());
        }
        let output = command.output().expect("run probe");
        assert!(
            output.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn network_off_refuses_inet_sockets_including_udp_while_unix_still_works() {
        if status() == Status::Unavailable {
            eprintln!("skipped: kernel does not provide Landlock");
            return;
        }
        let root = std::env::temp_dir().join(format!("agent-sandbox-net-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let probe = r#"import socket
for name, family in (("ipv4", socket.AF_INET), ("ipv6", socket.AF_INET6)):
    try:
        socket.socket(family, socket.SOCK_DGRAM)
        print(name, "allowed")
    except PermissionError:
        print(name, "denied")
    except OSError:
        print(name, "unsupported")
socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
print("unix", "allowed")
"#;
        let readable = system_read_roots();
        let denied = ChildSandbox::new(std::slice::from_ref(&root), &readable, false)
            .expect("ruleset")
            .expect("supported kernel");
        assert_eq!(
            probe_under(denied, probe),
            vec!["ipv4 denied", "ipv6 denied", "unix allowed"]
        );
        // Control: with network allowed the same probe creates the IPv4 socket,
        // so the denials above come from the filter and not from the probe.
        let allowed = ChildSandbox::new(std::slice::from_ref(&root), &readable, true)
            .expect("ruleset")
            .expect("supported kernel");
        assert_eq!(probe_under(allowed, probe)[0], "ipv4 allowed");
        let _ = std::fs::remove_dir_all(root);
    }
}
