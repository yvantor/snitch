//! Engine for dynamic binary translation and execution

use crate::{riscv, tran::ElfTranslator, util::SiUnit};
use anyhow::{anyhow, bail, Result};
use itertools::Itertools;
use llvm_sys::{
    core::*, execution_engine::*, ir_reader::*, linker::*, prelude::*, support::*,
    transforms::pass_manager_builder::*,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
};

pub use crate::runtime::{DmaState, SsrState};

/// An execution engine.
pub struct Engine {
    /// The global LLVM context.
    pub context: LLVMContextRef,
    /// The LLVM module which contains the translated code.
    pub module: LLVMModuleRef,
    /// The exit code set by the binary.
    pub exit_code: AtomicU32,
    /// Whether an error occurred during execution.
    pub had_error: AtomicBool,
    /// Optimize the LLVM IR.
    pub opt_llvm: bool,
    /// Optimize during JIT compilation.
    pub opt_jit: bool,
    /// Enable instruction tracing.
    pub trace: bool,
    /// The base hartid.
    pub base_hartid: usize,
    /// The number of cores.
    pub num_cores: usize,
    /// The number of clusters.
    pub num_clusters: usize,
    /// The global memory.
    pub memory: Mutex<HashMap<u64, u32>>,
}

// SAFETY: This is safe because only `context` and `module`
unsafe impl std::marker::Send for Engine {}
unsafe impl std::marker::Sync for Engine {}

impl Engine {
    /// Create a new execution engine.
    pub fn new(context: LLVMContextRef) -> Self {
        // Create a new LLVM module ot compile into.
        let module = unsafe {
            // Wrap the runtime IR up in an LLVM memory buffer.
            let mut initial_ir = crate::runtime::JIT_INITIAL.to_vec();
            initial_ir.push(0); // somehow this is needed despite RequireNullTerminated=0 below
            let initial_buf = LLVMCreateMemoryBufferWithMemoryRange(
                initial_ir.as_ptr() as *const _,
                initial_ir.len() - 1,
                b"jit.ll\0".as_ptr() as *const _,
                0,
            );

            // Parse the module.
            let mut module = std::mem::MaybeUninit::uninit().assume_init();
            let mut errmsg = std::mem::MaybeUninit::zeroed().assume_init();
            if LLVMParseIRInContext(context, initial_buf, &mut module, &mut errmsg) != 0
                || !errmsg.is_null()
            {
                error!(
                    "Cannot parse `jit.ll` IR: {:?}",
                    std::ffi::CStr::from_ptr(errmsg)
                );
            }

            // let module =
            //     LLVMModuleCreateWithNameInContext(b"banshee\0".as_ptr() as *const _, context);
            // LLVMSetDataLayout(module, b"i8:8-i16:16-i32:32-i64:64\0".as_ptr() as *const _);
            module
        };

        // Wrap everything up in an engine struct.
        Self {
            context,
            module,
            exit_code: Default::default(),
            had_error: Default::default(),
            opt_llvm: true,
            opt_jit: true,
            trace: false,
            base_hartid: 0,
            num_cores: 1,
            num_clusters: 1,
            memory: Default::default(),
        }
    }

    /// Translate an ELF binary.
    pub fn translate_elf(&self, elf: &elf::File) -> Result<()> {
        let mut tran = ElfTranslator::new(elf, self);

        // Dump the contents of the binary.
        debug!("Loading ELF binary");
        for section in tran.sections() {
            debug!(
                "Loading ELF section `{}` from 0x{:x} to 0x{:x}",
                section.shdr.name,
                section.shdr.addr,
                section.shdr.addr + section.shdr.size
            );
            for (addr, inst) in tran.instructions(section) {
                trace!("  - 0x{:x}: {}", addr, inst);
            }
        }

        // Estimate the branch target addresses.
        tran.update_target_addrs();

        // Translate the binary.
        tran.translate()?;

        // Optimize the translation.
        if self.opt_llvm {
            unsafe { self.optimize() };
        }

        // Load and link the LLVM IR for the `jit.rs` runtime library.
        unsafe {
            let mut runtime_ir = crate::runtime::JIT_GENERATED.to_vec();
            runtime_ir.push(0); // somehow this is needed despite RequireNullTerminated=0 below
            let runtime_buf = LLVMCreateMemoryBufferWithMemoryRange(
                runtime_ir.as_ptr() as *const _,
                runtime_ir.len() - 1,
                b"jit.rs\0".as_ptr() as *const _,
                0,
            );

            // Parse the module.
            let mut runtime = std::mem::MaybeUninit::uninit().assume_init();
            let mut errmsg = std::mem::MaybeUninit::zeroed().assume_init();
            if LLVMParseIRInContext(self.context, runtime_buf, &mut runtime, &mut errmsg) != 0
                || !errmsg.is_null()
            {
                error!(
                    "Cannot parse `jit.rs` IR: {:?}",
                    std::ffi::CStr::from_ptr(errmsg)
                );
            }

            // Link the runtime module into the translated binary module.
            LLVMLinkModules2(self.module, runtime);
        };

        // Copy the executable sections into memory.
        {
            let mut mem = self.memory.lock().unwrap();
            for section in &elf.sections {
                if (section.shdr.flags.0 & elf::types::SHF_ALLOC.0) == 0 {
                    continue;
                }
                use byteorder::{LittleEndian, ReadBytesExt};
                trace!("Preloading ELF section `{}`", section.shdr.name);
                mem.extend(
                    section
                        .data
                        .chunks(4)
                        .enumerate()
                        .map(|(offset, mut value)| {
                            let addr = section.shdr.addr + offset as u64 * 4;
                            let value = value.read_u32::<LittleEndian>().unwrap_or(0);
                            trace!("  - 0x{:x} = 0x{:x}", addr, value);
                            (addr, value)
                        }),
                );
            }
        }

        Ok(())
    }

    unsafe fn optimize(&self) {
        debug!("Optimizing IR");
        let mpm = LLVMCreatePassManager();
        // let fpm = LLVMCreateFunctionPassManagerForModule(self.module);

        trace!("Populating pass managers");
        let pmb = LLVMPassManagerBuilderCreate();
        LLVMPassManagerBuilderSetOptLevel(pmb, 3);
        // LLVMPassManagerBuilderPopulateFunctionPassManager(pmb, fpm);
        LLVMPassManagerBuilderPopulateModulePassManager(pmb, mpm);
        LLVMPassManagerBuilderDispose(pmb);

        // trace!("Optimizing function");
        // let func = LLVMGetNamedFunction(self.module, "execute_binary\0".as_ptr() as *const _);
        // LLVMInitializeFunctionPassManager(fpm);
        // LLVMRunFunctionPassManager(fpm, func);
        // LLVMFinalizeFunctionPassManager(fpm);

        trace!("Optimizing entire module");
        LLVMRunPassManager(mpm, self.module);

        LLVMDisposePassManager(mpm);
        // LLVMDisposePassManager(fpm);
    }

    // Execute the loaded memory.
    pub fn execute(&self) -> Result<u32> {
        unsafe { self.execute_inner() }
    }

    unsafe fn execute_inner<'b>(&'b self) -> Result<u32> {
        // Create a JIT compiler for the module (and consumes it).
        debug!("Creating JIT compiler for translated code");
        let mut ee = std::mem::MaybeUninit::uninit().assume_init();
        let mut errmsg = std::mem::MaybeUninit::zeroed().assume_init();
        let optlevel = if self.opt_jit { 3 } else { 0 };
        LLVMCreateJITCompilerForModule(&mut ee, self.module, optlevel, &mut errmsg);
        if !errmsg.is_null() {
            bail!(
                "Cannot create JIT compiler: {:?}",
                std::ffi::CStr::from_ptr(errmsg)
            )
        }

        // Lookup the function which executes the binary.
        let exec: for<'c> extern "C" fn(&'c Cpu<'b, 'c>) = std::mem::transmute(
            LLVMGetFunctionAddress(ee, b"execute_binary\0".as_ptr() as *const _),
        );
        debug!("Translated binary is at {:?}", exec as *const i8);

        // Allocate some TCDM memories.
        let tcdms: Vec<_> = {
            let mut tcdm = vec![0u32; 128 * 1024 / 4];
            for (&addr, &value) in self.memory.lock().unwrap().iter() {
                if addr < 0x020000 {
                    tcdm[(addr / 4) as usize] = value;
                }
            }
            (0..self.num_clusters).map(|_| tcdm.clone()).collect()
        };

        // Create the CPUs.
        let cpus: Vec<_> = (0..self.num_clusters)
            .flat_map(|j| (0..self.num_cores).map(move |i| (j, i)))
            .map(|(j, i)| {
                let base_hartid = self.base_hartid + j * self.num_cores;
                Cpu::new(
                    self,
                    &tcdms[j][0],
                    base_hartid + i,
                    self.num_cores,
                    base_hartid,
                )
            })
            .collect();
        trace!(
            "Initial state hart {}: {:#?}",
            cpus[0].hartid,
            cpus[0].state
        );

        // Execute the binary.
        info!("Launching binary on {} harts", cpus.len());
        let t0 = std::time::Instant::now();
        crossbeam_utils::thread::scope(|s| {
            for cpu in &cpus {
                s.spawn(move |_| {
                    exec(cpu);
                    debug!("Hart {} finished", cpu.hartid);
                });
            }
        })
        .unwrap();
        let t1 = std::time::Instant::now();
        let duration = (t1.duration_since(t0)).as_secs_f64();
        debug!("All {} harts finished", cpus.len());

        // Count the number of instructions that we have retired.
        let instret: u64 = cpus.iter().map(|cpu| cpu.state.instret).sum();

        // Print some final statistics.
        trace!("Final state hart {}: {:#?}", cpus[0].hartid, cpus[0].state);
        info!(
            "Exit code is 0x{:x}",
            self.exit_code.load(Ordering::SeqCst) >> 1
        );
        info!(
            "Retired {} ({}) in {}, {}",
            instret,
            (instret as isize).si_unit("inst"),
            duration.si_unit("s"),
            (instret as f64 / duration).si_unit("inst/s"),
        );
        if self.had_error.load(Ordering::SeqCst) {
            Err(anyhow!("Encountered an error during execution"))
        } else {
            Ok(self.exit_code.load(Ordering::SeqCst) >> 1)
        }
    }
}

pub unsafe fn add_llvm_symbols() {
    LLVMAddSymbol(
        b"banshee_load\0".as_ptr() as *const _,
        Cpu::binary_load as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_store\0".as_ptr() as *const _,
        Cpu::binary_store as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_csr_read\0".as_ptr() as *const _,
        Cpu::binary_csr_read as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_csr_write\0".as_ptr() as *const _,
        Cpu::binary_csr_write as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_abort_escape\0".as_ptr() as *const _,
        Cpu::binary_abort_escape as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_abort_illegal_inst\0".as_ptr() as *const _,
        Cpu::binary_abort_illegal_inst as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_abort_illegal_branch\0".as_ptr() as *const _,
        Cpu::binary_abort_illegal_branch as *mut _,
    );
    LLVMAddSymbol(
        b"banshee_trace\0".as_ptr() as *const _,
        Cpu::binary_trace as *mut _,
    );
}

// /// A representation of the system state.
// #[repr(C)]
// pub struct System<'a> {}

/// A CPU pointer to be passed to the binary code.
#[repr(C)]
pub struct Cpu<'a, 'b> {
    engine: &'a Engine,
    state: CpuState,
    tcdm_ptr: &'b u32,
    hartid: usize,
    num_cores: usize,
    cluster_base_hartid: usize,
}

impl<'a, 'b> Cpu<'a, 'b> {
    /// Create a new CPU in a default state.
    pub fn new(
        engine: &'a Engine,
        tcdm_ptr: &'b u32,
        hartid: usize,
        num_cores: usize,
        cluster_base_hartid: usize,
    ) -> Self {
        Self {
            engine,
            state: Default::default(),
            tcdm_ptr,
            hartid,
            num_cores,
            cluster_base_hartid,
        }
    }

    fn binary_load(&self, addr: u32, size: u8) -> u32 {
        trace!("Load 0x{:x} ({}B)", addr, 8 << size);
        match addr {
            0x40000000 => 0x000000,                                     // tcdm_start
            0x40000008 => 0x020000,                                     // tcdm_end
            0x40000010 => self.num_cores as u32,                        // nr_cores
            0x40000020 => self.engine.exit_code.load(Ordering::SeqCst), // scratch_reg
            0x40000040 => self.cluster_base_hartid as u32,              // cluster_base_hartid
            _ => self
                .engine
                .memory
                .lock()
                .unwrap()
                .get(&(addr as u64))
                .copied()
                .unwrap_or(0),
        }
    }

    fn binary_store(&self, addr: u32, value: u32, size: u8) {
        trace!("Store 0x{:x} = 0x{:x} ({}B)", addr, value, 8 << size);
        match addr {
            0x40000000 => (),                                                   // tcdm_start
            0x40000008 => (),                                                   // tcdm_end
            0x40000010 => (),                                                   // nr_cores
            0x40000020 => self.engine.exit_code.store(value, Ordering::SeqCst), // scratch_reg
            0x40000040 => (), // cluster_base_hartid
            _ => {
                self.engine
                    .memory
                    .lock()
                    .unwrap()
                    .insert(addr as u64, value);
            }
        }
    }

    fn binary_csr_read(&self, csr: u16) -> u32 {
        trace!("Read CSR 0x{:x}", csr);
        match csr {
            0x7C0 => self.state.ssr_enable,
            0xF14 => self.hartid as u32, // mhartid
            _ => 0,
        }
    }

    fn binary_csr_write(&mut self, csr: u16, value: u32) {
        trace!("Write CSR 0x{:x} = 0x{:?}", csr, value);
        match csr {
            0x7C0 => self.state.ssr_enable = value,
            _ => (),
        }
    }

    fn binary_abort_escape(&self, addr: u32) {
        error!("CPU escaped binary at 0x{:x}", addr);
        self.engine.had_error.store(true, Ordering::SeqCst);
    }

    fn binary_abort_illegal_inst(&self, addr: u32, inst_raw: u32) {
        error!(
            "Illegal instruction {} at 0x{:x}",
            riscv::parse_u32(inst_raw),
            addr
        );
        self.engine.had_error.store(true, Ordering::SeqCst);
    }

    fn binary_abort_illegal_branch(&self, addr: u32, target: u32) {
        error!(
            "Branch to unpredicted address 0x{:x} at 0x{:x}",
            target, addr
        );
        self.engine.had_error.store(true, Ordering::SeqCst);
    }

    fn binary_trace(&self, addr: u32, inst: u32, accesses: &[TraceAccess], data: &[u64]) {
        // Assemble the arguments.
        let args = accesses.iter().copied().zip(data.iter().copied());
        let mut args = args.map(|(access, data)| match access {
            TraceAccess::ReadMem => format!("RA:{:08x}", data as u32),
            TraceAccess::WriteMem => format!("WA:{:08x}", data as u32),
            TraceAccess::ReadReg(x) => format!("x{}:{:08x}", x, data as u32),
            TraceAccess::WriteReg(x) => format!("x{}={:08x}", x, data as u32),
            TraceAccess::ReadFReg(x) => format!("f{}:{:016x}", x, data),
            TraceAccess::WriteFReg(x) => format!("f{}={:016x}", x, data),
        });
        let args = args.join(" ");

        // Assemble the trace line.
        let line = format!(
            "{:08} {:04} {:08x}  {:38}  # DASM({:08x})",
            self.state.instret, self.hartid, addr, args, inst
        );
        println!("{}", line);
    }
}

/// A representation of a single CPU core's state.
#[derive(Default)]
#[repr(C)]
pub struct CpuState {
    regs: [u32; 32],
    fregs: [u64; 32],
    pc: u32,
    instret: u64,
    ssrs: [SsrState; 2],
    ssr_enable: u32,
    dma: DmaState,
}

impl std::fmt::Debug for CpuState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let regs = self
            .regs
            .iter()
            .copied()
            .enumerate()
            .map(|(i, value)| format!("x{:02}: 0x{:08x}", i, value))
            .chunks(4)
            .into_iter()
            .map(|mut chunk| chunk.join("  "))
            .join("\n");
        let fregs = self
            .fregs
            .iter()
            .copied()
            .enumerate()
            .map(|(i, value)| format!("f{:02}: 0x{:016x}", i, value))
            .chunks(4)
            .into_iter()
            .map(|mut chunk| chunk.join("  "))
            .join("\n");
        f.debug_struct("CpuState")
            .field("regs", &format_args!("\n{}", regs))
            .field("fregs", &format_args!("\n{}", fregs))
            .field("pc", &format_args!("0x{:x}", self.pc))
            .field("instret", &self.instret)
            .field("ssrs", &self.ssrs)
            .field("dma", &self.dma)
            .finish()
    }
}

/// A single register or memory access as recorded in a trace.
#[derive(Debug, Clone, Copy)]
#[repr(C, u8)]
pub enum TraceAccess {
    ReadMem,
    ReadReg(u8),
    ReadFReg(u8),
    WriteMem,
    WriteReg(u8),
    WriteFReg(u8),
}