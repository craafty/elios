//! Build and run eliOS. Invoked as `cargo xtask <command>`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: cargo xtask <command> [options] [-- <extra qemu args>]

commands:
  build        build eliboot and the kernel, and lay out the boot volume
  run          build, then boot eliOS in QEMU
  test         boot a test build of the kernel in QEMU and run its tests

options:
  --release    build with optimizations
  --headless   no QEMU window; serial output only (run; test is always headless)
  --gdb        wait for a debugger on localhost:1234 (run, test)
  --trace      log interrupts and CPU resets to target/qemu.log (run, test)
";

const BOOTLOADER_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
/// User programs are Linux programs: eliOS implements the Linux system call
/// ABI.
const USER_TARGET: &str = "x86_64-unknown-linux-none";

// The commands eliutils provides (shared with eliutils itself).
include!("../../user/utils/src/applets.rs");

/// Where distributions install OVMF, the open-source UEFI firmware for QEMU.
/// QEMU exit statuses from the test kernel's `isa-debug-exit` writes
/// (see kernel/src/test.rs): QEMU exits with `(value << 1) | 1`.
const TESTS_PASSED: i32 = (0x10 << 1) | 1;
const TESTS_FAILED: i32 = (0x11 << 1) | 1;
const TEST_TIMEOUT: Duration = Duration::from_secs(120);

const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF.fd",
    "/usr/share/qemu/OVMF.fd",
    "/usr/share/edk2/x64/OVMF.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/edk2/ovmf/OVMF.fd",
];

#[derive(Default)]
struct Options {
    release: bool,
    headless: bool,
    gdb: bool,
    trace: bool,
    qemu_args: Vec<String>,
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let command = args.next();
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => return fail(&error),
    };

    let result = match command.as_deref() {
        Some("build") => build(&options).map(|_| ()),
        Some("run") => build(&options).and_then(|esp| run(&esp, &options)),
        Some("test") => test(&options),
        Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unknown or missing command\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

fn fail(error: &str) -> ExitCode {
    eprintln!("xtask: {error}");
    ExitCode::FAILURE
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--release" => options.release = true,
            "--headless" => options.headless = true,
            "--gdb" => options.gdb = true,
            "--trace" => options.trace = true,
            "--" => options.qemu_args.extend(args.by_ref()),
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn profile(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

/// Builds everything and lays out the boot volume. Returns its directory.
fn build(options: &Options) -> Result<PathBuf, String> {
    let bootloader = build_bootloader(options.release)?;
    cargo(&["build", "--package", "elikernel", "--target", KERNEL_TARGET], options.release, &[])?;
    let kernel = workspace_root().join("target").join(KERNEL_TARGET).join(profile(options.release)).join("elikernel");
    let initrd = build_initrd()?;
    make_boot_volume("esp", &bootloader, &kernel, &initrd)
}

fn test(options: &Options) -> Result<(), String> {
    let bootloader = build_bootloader(options.release)?;
    let kernel = build_test_kernel(options.release)?;
    let initrd = build_initrd()?;
    let esp = make_boot_volume("esp-test", &bootloader, &kernel, &initrd)?;

    let mut qemu = qemu_command(&esp, options)?;
    qemu.args(["-display", "none", "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    let status = run_with_timeout(qemu, TEST_TIMEOUT)?;
    match status.code() {
        Some(TESTS_PASSED) => Ok(()),
        Some(TESTS_FAILED) => Err("kernel tests failed".into()),
        _ => Err(format!("test kernel stopped without reporting a result (QEMU {status})")),
    }
}

fn build_bootloader(release: bool) -> Result<PathBuf, String> {
    cargo(&["build", "--package", "eliboot", "--target", BOOTLOADER_TARGET], release, &[])?;
    Ok(workspace_root().join("target").join(BOOTLOADER_TARGET).join(profile(release)).join("eliboot.efi"))
}

/// Builds the kernel with cfg(test) and returns the path of the binary.
fn build_test_kernel(release: bool) -> Result<PathBuf, String> {
    let stdout = cargo(
        &[
            "test",
            "--no-run",
            "--package",
            "elikernel",
            "--bin",
            "elikernel",
            "--target",
            KERNEL_TARGET,
            "--message-format=json-render-diagnostics",
        ],
        release,
        &[],
    )?;
    // Cargo prints one JSON object per line; the test binary is the artifact
    // built with the test profile.
    stdout
        .lines()
        .filter(|line| line.contains(r#""reason":"compiler-artifact""#) && line.contains(r#""test":true"#))
        .find_map(|line| json_string_field(line, "executable"))
        .map(PathBuf::from)
        .ok_or_else(|| "cargo didn't report a test kernel binary".into())
}

/// Extracts a string field from one line of JSON. Just enough JSON for
/// cargo's output; xtask has no dependencies, so no JSON library.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let start = json.find(&format!(r#""{field}":""#))? + field.len() + 4;
    let mut value = String::new();
    let mut chars = json[start..].chars();
    loop {
        match chars.next()? {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                c => value.push(c),
            },
            c => value.push(c),
        }
    }
}

/// Runs cargo in the workspace and returns its standard output (diagnostics
/// still go to the terminal).
fn cargo(args: &[&str], release: bool, environment: &[(&str, &str)]) -> Result<String, String> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(workspace_root()).args(args).envs(environment.iter().copied()).stderr(Stdio::inherit());
    if release {
        command.arg("--release");
    }
    let output = command.output().map_err(|e| format!("running cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!("`cargo {}` failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Copies the bootloader, kernel and initrd into an EFI system partition
/// layout under `target/<name>`.
fn make_boot_volume(name: &str, bootloader: &Path, kernel: &Path, initrd: &Path) -> Result<PathBuf, String> {
    let esp = workspace_root().join("target").join(name);
    let files = [
        (bootloader, esp.join("EFI/BOOT/BOOTX64.EFI")),
        (kernel, esp.join("EFI/eliOS/kernel.elf")),
        (initrd, esp.join("EFI/eliOS/initrd.cpio")),
    ];
    for (from, to) in &files {
        fs::create_dir_all(to.parent().unwrap()).map_err(|e| format!("creating {}: {e}", to.display()))?;
        fs::copy(from, to).map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
    }
    Ok(esp)
}

/// Builds the user programs (always optimized and without debug info: they
/// go into the initrd, where size matters).
fn build_userland() -> Result<PathBuf, String> {
    cargo(
        &[
            "build",
            "--package",
            "eliutils",
            "--target",
            USER_TARGET,
            // Tier 3 target: the standard library is built from source.
            "-Zbuild-std=core,alloc",
            "-Zbuild-std-features=compiler-builtins-mem",
        ],
        true,
        &[("CARGO_PROFILE_RELEASE_DEBUG", "false"), ("CARGO_PROFILE_RELEASE_STRIP", "debuginfo")],
    )?;
    Ok(workspace_root().join("target").join(USER_TARGET).join("release").join("eliutils"))
}

/// Assembles the root file system in `target/rootfs` (the repository's
/// `rootfs/` directory plus the programs) and packs it into a cpio archive.
fn build_initrd() -> Result<PathBuf, String> {
    let eliutils = build_userland()?;
    let root = workspace_root();
    let rootfs = root.join("target/rootfs");
    if rootfs.exists() {
        fs::remove_dir_all(&rootfs).map_err(|e| format!("clearing {}: {e}", rootfs.display()))?;
    }
    copy_tree(&root.join("rootfs"), &rootfs)?;

    let bin = rootfs.join("bin");
    let sbin = rootfs.join("sbin");
    for directory in [&bin, &sbin] {
        fs::create_dir_all(directory).map_err(|e| format!("creating {}: {e}", directory.display()))?;
    }
    fs::copy(&eliutils, bin.join("eliutils")).map_err(|e| format!("copying eliutils: {e}"))?;
    for (name, _) in APPLETS {
        symlink("eliutils", &bin.join(name))?;
    }
    symlink("/bin/eliutils", &sbin.join("init"))?;

    let mut archive = Vec::new();
    let mut inode = 1;
    add_to_cpio(&mut archive, &rootfs, "", &mut inode)?;
    cpio_entry(&mut archive, 0, "TRAILER!!!", 0, &[]);
    let path = root.join("target/initrd.cpio");
    fs::write(&path, archive).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

fn symlink(target: &str, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("linking {}: {e}", link.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("creating {}: {e}", to.display()))?;
    let entries = fs::read_dir(from).map_err(|e| format!("reading {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| format!("copying {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Adds a directory tree to a cpio archive, parents before children, in a
/// stable order.
fn add_to_cpio(archive: &mut Vec<u8>, directory: &Path, prefix: &str, inode: &mut u32) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .map_err(|e| format!("reading {}: {e}", directory.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        *inode += 1;
        if kind.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|e| e.to_string())?;
            cpio_entry(archive, *inode, &name, 0o120777, target.to_string_lossy().as_bytes());
        } else if kind.is_dir() {
            cpio_entry(archive, *inode, &name, 0o040755, &[]);
            add_to_cpio(archive, &entry.path(), &format!("{name}/"), inode)?;
        } else {
            let data = fs::read(entry.path()).map_err(|e| format!("reading {}: {e}", entry.path().display()))?;
            let mode = if name.starts_with("bin/") { 0o100755 } else { 0o100644 };
            cpio_entry(archive, *inode, &name, mode, &data);
        }
    }
    Ok(())
}

/// One entry in the cpio "newc" format: a header of 8-digit hex fields, the
/// name, the data, each padded to 4 bytes.
fn cpio_entry(archive: &mut Vec<u8>, inode: u32, name: &str, mode: u32, data: &[u8]) {
    let fields = [inode, mode, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
    archive.extend_from_slice(b"070701");
    for field in fields {
        archive.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    archive.resize(archive.len().next_multiple_of(4), 0);
    archive.extend_from_slice(data);
    archive.resize(archive.len().next_multiple_of(4), 0);
}

fn run(esp: &Path, options: &Options) -> Result<(), String> {
    let mut qemu = qemu_command(esp, options)?;
    if options.headless {
        qemu.args(["-display", "none"]);
    }
    let status = qemu.status().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    if !status.success() {
        return Err(format!("QEMU exited with {status}"));
    }
    Ok(())
}

/// The QEMU invocation shared by `run` and `test`.
fn qemu_command(esp: &Path, options: &Options) -> Result<Command, String> {
    let firmware = find_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args(["-machine", "q35", "-m", "256M", "-nic", "none"])
        .arg("-bios")
        .arg(&firmware)
        // Exposes the directory to the guest as a FAT disk, so there's no
        // image to rebuild while iterating.
        .arg("-drive")
        .arg(format!("format=raw,file=fat:rw:{}", esp.display()))
        .args(["-serial", "stdio"])
        // A triple fault should stop the machine, not quietly reboot it.
        .arg("-no-reboot");
    if options.gdb {
        qemu.args(["-s", "-S"]);
        eprintln!("xtask: waiting for gdb on localhost:1234");
    }
    if options.trace {
        let log = workspace_root().join("target/qemu.log");
        qemu.args(["-d", "int,cpu_reset", "-D"]).arg(log);
    }
    qemu.args(&options.qemu_args);
    Ok(qemu)
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<ExitStatus, String> {
    let mut child = command.spawn().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("waiting for QEMU: {e}"))? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn find_ovmf() -> Result<PathBuf, String> {
    if let Ok(path) = env::var("OVMF_PATH") {
        return Ok(path.into());
    }
    OVMF_CANDIDATES.iter().map(PathBuf::from).find(|path| path.exists()).ok_or_else(|| {
        "couldn't find OVMF UEFI firmware; install your distribution's `ovmf` package or set OVMF_PATH".into()
    })
}
