/// microvm-compat: Host compatibility checker for the microvm runtime.
///
/// Exercises the exact KVM code paths the VMM uses:
///   /dev/kvm access, required capabilities, VM creation,
///   irqchip/PIT/TSS, guest memory, vCPU + CPUID/sregs/msrs,
///   and network device availability (macvtap/tun).

use std::process::ExitCode;

use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{Cap, Kvm};
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

const GUEST_MEM_SIZE: usize = 256 * 1024 * 1024;

fn main() -> ExitCode {
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut warn = 0u32;

    macro_rules! check {
        ($name:expr, $body:expr) => {{
            match (|| -> Result<String, Box<dyn std::error::Error>> { $body })() {
                Ok(detail) => {
                    pass += 1;
                    println!("  \x1b[32mPASS\x1b[0m  {}{}", $name,
                        if detail.is_empty() { String::new() } else { format!(" ({})", detail) });
                }
                Err(e) => {
                    fail += 1;
                    println!("  \x1b[31mFAIL\x1b[0m  {} — {}", $name, e);
                }
            }
        }};
    }

    macro_rules! advisory {
        ($name:expr, $body:expr) => {{
            match (|| -> Result<String, Box<dyn std::error::Error>> { $body })() {
                Ok(detail) => {
                    pass += 1;
                    println!("  \x1b[32mPASS\x1b[0m  {}{}", $name,
                        if detail.is_empty() { String::new() } else { format!(" ({})", detail) });
                }
                Err(e) => {
                    warn += 1;
                    println!("  \x1b[33mWARN\x1b[0m  {} — {}", $name, e);
                }
            }
        }};
    }

    println!("\nmicrovm host compatibility check\n");

    // ── KVM access ──────────────────────────────────────────────

    println!("KVM");

    check!("/dev/kvm accessible", {
        let meta = std::fs::metadata("/dev/kvm")
            .map_err(|e| format!("/dev/kvm: {e}"))?;
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode();
        Ok(format!("mode {:04o}", mode & 0o7777))
    });

    let kvm = match Kvm::new() {
        Ok(k) => {
            pass += 1;
            let api_ver = k.get_api_version();
            println!("  \x1b[32mPASS\x1b[0m  KVM_GET_API_VERSION ({})", api_ver);
            Some(k)
        }
        Err(e) => {
            fail += 1;
            println!("  \x1b[31mFAIL\x1b[0m  Kvm::new() — {e}");
            None
        }
    };

    if let Some(ref kvm) = kvm {
        // ── Required capabilities ───────────────────────────────

        println!("\nCapabilities (required)");

        check!("KVM_CAP_USER_MEMORY", {
            if kvm.check_extension(Cap::UserMemory) { Ok(String::new()) }
            else { Err("not supported".into()) }
        });

        check!("KVM_CAP_IRQCHIP", {
            if kvm.check_extension(Cap::Irqchip) { Ok(String::new()) }
            else { Err("not supported".into()) }
        });

        check!("KVM_CAP_SET_TSS_ADDR", {
            if kvm.check_extension(Cap::SetTssAddr) { Ok(String::new()) }
            else { Err("not supported".into()) }
        });

        // ── VM creation ─────────────────────────────────────────

        println!("\nVM creation");

        let vm = match kvm.create_vm() {
            Ok(v) => {
                pass += 1;
                println!("  \x1b[32mPASS\x1b[0m  KVM_CREATE_VM");
                Some(v)
            }
            Err(e) => {
                fail += 1;
                println!("  \x1b[31mFAIL\x1b[0m  KVM_CREATE_VM — {e}");
                None
            }
        };

        if let Some(ref vm) = vm {
            check!("KVM_SET_TSS_ADDR", {
                vm.set_tss_address(0xFFFB_D000)?;
                Ok(String::new())
            });

            check!("KVM_CREATE_IRQCHIP", {
                vm.create_irq_chip()?;
                Ok(String::new())
            });

            check!("KVM_CREATE_PIT2", {
                vm.create_pit2(kvm_pit_config::default())?;
                Ok(String::new())
            });

            // ── Guest memory ────────────────────────────────────

            println!("\nGuest memory");

            let guest_mem: Option<GuestMemoryMmap> = match GuestMemoryMmap::from_ranges(
                &[(GuestAddress(0), GUEST_MEM_SIZE)]
            ) {
                Ok(m) => {
                    pass += 1;
                    println!("  \x1b[32mPASS\x1b[0m  mmap {} MB guest RAM", GUEST_MEM_SIZE >> 20);
                    Some(m)
                }
                Err(e) => {
                    fail += 1;
                    println!("  \x1b[31mFAIL\x1b[0m  mmap guest RAM — {e}");
                    None
                }
            };

            if let Some(ref mem) = guest_mem {
                check!("KVM_SET_USER_MEMORY_REGION", {
                    for (i, region) in mem.iter().enumerate() {
                        let mr = kvm_userspace_memory_region {
                            slot: i as u32,
                            flags: 0,
                            guest_phys_addr: region.start_addr().raw_value(),
                            memory_size: region.len(),
                            userspace_addr: mem.get_host_address(region.start_addr())? as u64,
                        };
                        unsafe { vm.set_user_memory_region(mr)?; }
                    }
                    Ok(String::new())
                });
            }

            // ── vCPU ────────────────────────────────────────────

            println!("\nvCPU");

            let vcpu = match vm.create_vcpu(0) {
                Ok(v) => {
                    pass += 1;
                    println!("  \x1b[32mPASS\x1b[0m  KVM_CREATE_VCPU");
                    Some(v)
                }
                Err(e) => {
                    fail += 1;
                    println!("  \x1b[31mFAIL\x1b[0m  KVM_CREATE_VCPU — {e}");
                    None
                }
            };

            if let Some(ref vcpu) = vcpu {
                check!("KVM_SET_CPUID2 (host passthrough)", {
                    let cpuid = kvm.get_supported_cpuid(256)?;
                    vcpu.set_cpuid2(&cpuid)?;
                    let n = cpuid.as_slice().len();
                    Ok(format!("{n} entries"))
                });

                check!("KVM_SET_SREGS (long mode)", {
                    let mut sregs = vcpu.get_sregs()?;
                    sregs.cr0 = 0x8005_0033;
                    sregs.cr4 = 0x0000_06A0;
                    sregs.efer = 0x0000_0D01;
                    vcpu.set_sregs(&sregs)?;
                    Ok(String::new())
                });

                check!("KVM_SET_REGS", {
                    use kvm_bindings::kvm_regs;
                    let regs = kvm_regs {
                        rip: 0x100000,
                        rflags: 0x2,
                        rsp: 0x8ff0,
                        rbp: 0x8ff0,
                        ..Default::default()
                    };
                    vcpu.set_regs(&regs)?;
                    Ok(String::new())
                });

                check!("KVM_SET_FPU", {
                    use kvm_bindings::kvm_fpu;
                    let fpu = kvm_fpu {
                        fcw: 0x37f,
                        mxcsr: 0x1f80,
                        ..Default::default()
                    };
                    vcpu.set_fpu(&fpu)?;
                    Ok(String::new())
                });

                check!("KVM_SET_MSRS (10 entries)", {
                    use kvm_bindings::{kvm_msr_entry, Msrs};
                    let entries = [
                        kvm_msr_entry { index: 0x174, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0x175, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0x176, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0xc000_0081, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0xc000_0082, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0xc000_0083, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0xc000_0084, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0xc000_0102, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0x10, data: 0, ..Default::default() },
                        kvm_msr_entry { index: 0x1a0, data: 1, ..Default::default() },
                    ];
                    let mut msrs = Msrs::new(entries.len())?;
                    msrs.as_mut_slice().copy_from_slice(&entries);
                    vcpu.set_msrs(&msrs)?;
                    Ok(String::new())
                });

                check!("KVM_IRQ_LINE (edge trigger)", {
                    vm.set_irq_line(5, true)?;
                    vm.set_irq_line(5, false)?;
                    Ok(String::new())
                });
            }
        }
    }

    // ── Network devices ─────────────────────────────────────────

    println!("\nNetwork (advisory)");

    advisory!("macvtap kernel module", {
        let mods = std::fs::read_to_string("/proc/modules")
            .unwrap_or_default();
        if mods.contains("macvtap") || mods.contains("macvlan") {
            return Ok("loaded".into());
        }
        // Check if built-in
        let builtins = std::fs::read_to_string(
            format!("/lib/modules/{}/modules.builtin",
                nix::sys::utsname::uname()?.release().to_str().unwrap_or(""))
        ).unwrap_or_default();
        if builtins.contains("macvtap") {
            return Ok("built-in".into());
        }
        // Check if loadable
        let modinfo = std::process::Command::new("modinfo")
            .arg("macvtap")
            .output();
        match modinfo {
            Ok(out) if out.status.success() => Ok("available (not loaded)".into()),
            _ => Err("module not found — macvtap networking unavailable".into()),
        }
    });

    advisory!("/dev/net/tun (for TAP fallback)", {
        std::fs::metadata("/dev/net/tun")
            .map_err(|e| format!("/dev/net/tun: {e}"))?;
        Ok(String::new())
    });

    advisory!("CAP_NET_ADMIN", {
        // Try creating a dummy socket to test network admin caps
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            return Err("cannot create socket".into());
        }
        // Try SIOCGIFFLAGS on lo as a proxy for net admin
        #[repr(C)]
        struct Ifreq {
            ifr_name: [u8; 16],
            ifr_flags: i16,
            _pad: [u8; 22],
        }
        let mut ifr = Ifreq { ifr_name: [0; 16], ifr_flags: 0, _pad: [0; 22] };
        ifr.ifr_name[..2].copy_from_slice(b"lo");
        let ret = unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) };
        unsafe { libc::close(sock); }
        if ret < 0 {
            Err("cannot query interface flags — likely missing CAP_NET_ADMIN".into())
        } else {
            Ok(String::new())
        }
    });

    // ── Host info ───────────────────────────────────────────────

    println!("\nHost info");

    if let Ok(uts) = nix::sys::utsname::uname() {
        println!("  kernel:  {} {}", uts.sysname().to_str().unwrap_or("?"),
            uts.release().to_str().unwrap_or("?"));
        println!("  arch:    {}", uts.machine().to_str().unwrap_or("?"));
    }
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in info.lines() {
            if line.starts_with("model name") {
                let cpu = line.split(':').nth(1).map(|s| s.trim()).unwrap_or("?");
                println!("  cpu:     {cpu}");
                break;
            }
        }
        // Check for VMX/SVM flags
        let has_vmx = info.lines().any(|l| l.starts_with("flags") && l.contains(" vmx"));
        let has_svm = info.lines().any(|l| l.starts_with("flags") && l.contains(" svm"));
        if has_vmx {
            println!("  virt:    Intel VT-x (vmx)");
        } else if has_svm {
            println!("  virt:    AMD-V (svm)");
        } else {
            println!("  virt:    \x1b[33mno vmx/svm flags — nested or emulated?\x1b[0m");
        }
    }
    // Detect if running inside a VM/container
    if let Ok(dmi) = std::fs::read_to_string("/sys/class/dmi/id/product_name") {
        let product = dmi.trim();
        if !product.is_empty() {
            println!("  product: {product}");
        }
    }
    if let Ok(hyp) = std::fs::read_to_string("/sys/hypervisor/type") {
        println!("  hypervisor: {}", hyp.trim());
    }
    if std::fs::metadata("/.dockerenv").is_ok() {
        println!("  container: Docker detected (/.dockerenv)");
    } else if let Ok(cg) = std::fs::read_to_string("/proc/1/cgroup") {
        if cg.contains("docker") || cg.contains("kubepods") || cg.contains("containerd") {
            println!("  container: detected from cgroup");
        }
    }

    // ── Summary ─────────────────────────────────────────────────

    println!("\n─────────────────────────────────────");
    println!("  {pass} passed, {fail} failed, {warn} warnings\n");

    if fail > 0 {
        println!("  \x1b[31mThis host cannot run the microvm.\x1b[0m\n");
        ExitCode::from(1)
    } else if warn > 0 {
        println!("  \x1b[33mKVM OK — network features may be limited.\x1b[0m\n");
        ExitCode::from(0)
    } else {
        println!("  \x1b[32mFull compatibility.\x1b[0m\n");
        ExitCode::from(0)
    }
}
