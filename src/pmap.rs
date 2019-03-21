use crate::fdt::{self, MachineMeta};
use crate::context::Context;
use crate::memory_region::{MemoryRegion, PageTableRegion};
use crate::trap::MAX_STACK_PADDR;
use crate::{riscv, sum};
use core::ops::{Index, IndexMut};
use core::ptr;
use riscv_decode::Instruction;
use spin::Mutex;

const PAGE_SIZE: u64 = 4096;
const HPAGE_SIZE: u64 = 2 * 1024 * 1024;

const PAGE_TABLE_SHIFT: u32 = 9;

pub const SV39_MASK: u64 = !((!0) << 39);

#[allow(unused)]
mod pte_flags {
    pub const PTE_VALID: u64 = 0x1;
    pub const PTE_READ: u64 = 0x2;
    pub const PTE_WRITE: u64 = 0x4;
    pub const PTE_EXECUTE: u64 = 0x8;
    pub const PTE_USER: u64 = 0x10;
    pub const PTE_GLOBAL: u64 = 0x20;
    pub const PTE_ACCESSED: u64 = 0x40;
    pub const PTE_DIRTY: u64 = 0x80;
    pub const PTE_RSV_MASK: u64 = 0x300;

    pub const PTE_AD: u64 = PTE_ACCESSED | PTE_DIRTY;
    pub const PTE_RWXV: u64 = PTE_READ | PTE_WRITE | PTE_EXECUTE | PTE_VALID;
}
pub use pte_flags::*;

mod page_table_constants {
    pub const BOOT_PAGE_TABLE: u64 = 0x80017000;

    pub const DIRECT_MAP_PT_INDEX: u64 = 0xf80;
    pub const DIRECT_MAP_OFFSET: u64 = DIRECT_MAP_PT_INDEX << 27 | ((!0) << 39);
    pub const DIRECT_MAP_PAGES: u64 = 4; // Uses 1 GB pages
}
pub use page_table_constants::*;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PageTableRoot {
    UVA,
    KVA,
    MVA,
    MPA,
}
use PageTableRoot::*;

const NULL_PAGE_PTR: u64 = 2;

pub struct PageTables {
    region: PageTableRegion,
    root_page_tables: [u64; 4],
    free_list_head: u64,
}
impl PageTables {
    /// Create a set of page tables from a memory region.
    ///
    /// The `initrd_start` and `initrd_end` parameters are an unfortunate implementation detail: the
    /// bootloader might have placed the init RAM disk inside our page table region. If this
    /// happened, we must make sure not to mark those pages as free until we're done using it.
    pub fn new(region: MemoryRegion, initrd_start: u64, initrd_end: u64) -> Self {
        let start = region.base();
        let end = start + region.len();
        let region = PageTableRegion::new(region);

        let mut ret = Self {
            region,
            root_page_tables: [0, 0, 0, 0],
            free_list_head: NULL_PAGE_PTR,
        };

        // initialize free list
        assert_eq!(start % PAGE_SIZE, 0);
        let mut addr = start;
        while addr < end {
            if addr + PAGE_SIZE <= initrd_start || addr >= initrd_end {
                ret.free_page(addr);
            }

            addr += PAGE_SIZE;
        }

        // initialize root page tables
        for i in 0..4 {
            ret.root_page_tables[i] = ret.alloc_page();
        }

        ret
    }

    pub fn root_pa(&self, root: PageTableRoot) -> u64 {
        let i = match root {
            MPA => 0,
            UVA => 1,
            KVA => 2,
            MVA => 3,
        };
        self.root_page_tables[i]
    }

    pub fn install_root(&self, root: PageTableRoot) {
        let new_satp = (8 << 60) | (self.root_pa(root) >> 12);
        if csrr!(satp) != new_satp {
            csrw!(satp, new_satp);
            riscv::sfence_vma();
        }
    }

    pub fn set_mapping(&mut self, root: PageTableRoot, va: u64, pte: u64) {
        let pte_addr = self.pte_for_addr(root, va);
        self.region.set_leaf_pte(pte_addr, pte);
    }

    // Returns the physical address of the pte for a given virtual address.
    fn pte_for_addr(&mut self, root: PageTableRoot, va: u64) -> u64 {
        // These ranges use huge pages...
        assert!(va < DIRECT_MAP_OFFSET);
        assert!(is_sv39(va));
        assert!(root != PageTableRoot::MPA);

        let mut page_table = self.root_pa(root);
        for level in 0..2 {
            let pte_index = ((va >> (30 - PAGE_TABLE_SHIFT * level)) & 0x1ff);
            let pte_addr = page_table + pte_index * 8;
            let pte = self.region[pte_addr];

            if pte & PTE_VALID != 0 {
                assert_eq!(pte & (PTE_READ | PTE_WRITE | PTE_EXECUTE), 0);
                page_table = (pte >> 10) << 12;
            } else {
                let page = self.alloc_page();
                self.region.set_nonleaf_pte(pte_addr, (page >> 2) | PTE_VALID);
                page_table = page;
            }
        }
        page_table + ((va >> 12) & 0x1ff) * 8
    }

    pub fn clear_page_table_range(&mut self, pa: u64, start_index: u64, end_index: u64) {
        assert!(start_index <= end_index);
        assert!(end_index <= 512);

        for i in start_index..end_index {
            let pte = self.region[pa + i * 8];
            if pte & PTE_RWXV == PTE_VALID {
                let page = (pte >> 10) << 12;
                self.clear_page_table_range(page, 0, 512);
                self.free_page(page);
            }
            self.region.set_invalid_pte(pa + i * 8, 0);
        }
    }

    fn alloc_page(&mut self) -> u64 {
        if self.free_list_head == NULL_PAGE_PTR {
            panic!("Out of hypervisor memory for page tables");
        }

        let free = self.free_list_head;
        self.free_list_head = self.region[free];

        let mut addr = free;
        while addr < free + PAGE_SIZE {
            self.region.set_invalid_pte(addr, 0);
            addr += 8;
        }

        free
    }

    fn free_page(&mut self, page: u64) {
        self.region.set_invalid_pte(page, self.free_list_head);
        self.free_list_head = page;
    }
}

pub fn pa2va(pa: u64) -> u64 { pa + DIRECT_MAP_OFFSET }
pub fn va2pa(va: u64) -> u64 {
     // Must be in HPA region.
    assert!(va >= DIRECT_MAP_OFFSET);
    assert!(va < DIRECT_MAP_OFFSET + (DIRECT_MAP_PAGES<<30));
    va - DIRECT_MAP_OFFSET
}

/// Returns whether va is a sign extended 39 bit address
pub fn is_sv39(va: u64) -> bool {
    let shifted = va >> 38;
    shifted == 0 || shifted == 0x3ffffff
}
/// Returns whether va is a sign extended 48 bit address
pub fn is_sv48(va: u64) -> bool {
    let shifted = va >> 47;
    shifted == 0 || shifted == 0x1ffff
}

#[allow(unused)]
pub enum AccessType {
    Read,
    Write,
    Execute,
}

pub struct AddressTranslation {
    pub pte_value: u64,
    pub pte_addr: u64,
    pub guest_pa: u64,
}

// Returns the guest physical address associated with a given guest virtual address, by walking
// guest page tables.
pub fn translate_guest_address(guest_memory: &MemoryRegion, root_page_table: u64, addr: u64) -> Option<AddressTranslation> {
    if !is_sv39(addr) || root_page_table % PAGE_SIZE != 0 {
        return None;
    }

    let mut page_table = root_page_table;
    for level in 0..3 {
        let pte_index = ((addr >> (30 - 9 * level)) & 0x1ff);
        let pte_addr = page_table + pte_index * 8;
        let pte = guest_memory.get(pte_addr)?;

        if pte & PTE_VALID == 0 || ((pte & PTE_WRITE) != 0 && (pte & PTE_READ) == 0) {
            return None;
        } else if pte & (PTE_READ | PTE_EXECUTE) != 0 {
            let guest_pa = match level {
                2 => ((pte >> 10) << 12) | (addr & 0xfff),
                1 => ((pte >> 19) << 21) | (addr & 0x1fffff),
                0 => ((pte >> 28) << 30) | (addr & 0x3fffffff),
                _ => unreachable!(),
            };
            return Some(AddressTranslation { guest_pa, pte_addr, pte_value: pte });
        } else {
            page_table = (pte >> 10) << 12;
        }
    }

    None
}

pub fn init(machine: &MachineMeta) -> (PageTables, MemoryRegion) {
    unsafe {
        // Setup direct map region in boot page table
        for i in 0..DIRECT_MAP_PAGES {
            *((BOOT_PAGE_TABLE + DIRECT_MAP_PT_INDEX + i * 8) as *mut u64) = (i << 28) | PTE_AD | PTE_RWXV;
        }

        // Create guest memory region
        let guest_memory = MemoryRegion::with_base_address(
            pa2va(machine.gpm_offset + machine.guest_shift), machine.gpm_offset, machine.gpm_size);

        // Create shadow page tables
        let memory_region_length = machine.hpm_offset + fdt::VM_RESERVATION_SIZE - MAX_STACK_PADDR;
        let memory_region = MemoryRegion::new(pa2va(MAX_STACK_PADDR), memory_region_length);
        let mut shadow_page_tables = PageTables::new(memory_region, machine.initrd_start, machine.initrd_end);

        // Initialize shadow page tables
        for &root in &[MPA, UVA, KVA, MVA] {
            let pa = shadow_page_tables.root_pa(root);
            ptr::write_bytes(pa as *mut u8, 0, PAGE_SIZE as usize);

            // Direct map region
            for i in 0..DIRECT_MAP_PAGES {
                *((pa + DIRECT_MAP_PT_INDEX + i * 8) as *mut u64) = (i << 28) | PTE_AD | PTE_RWXV;
            }

            // Hypervisor code + data
            *((pa + 0xff8) as *mut u64) = 0x20000000 | PTE_AD | PTE_RWXV;
        }

        shadow_page_tables.install_root(MPA);
        crate::print::uart::UART = pa2va(crate::print::uart::UART as u64) as *mut _;

        assert_eq!(machine.gpm_offset, 0x80000000);

        // Map guest physical memory
        assert_eq!(machine.gpm_size % HPAGE_SIZE, 0);
        let root_pa = shadow_page_tables.root_pa(MPA);
        let npages = machine.gpm_size / HPAGE_SIZE;
        for p in 0..npages  {
            let va = machine.gpm_offset + p * HPAGE_SIZE;
            let pa = va + machine.guest_shift;

            let pte_index = va >> 30;
            let pte_addr = root_pa + pte_index * 8;
            let pte = shadow_page_tables.region[pte_addr];
            let page_table = if pte & PTE_VALID != 0 {
                assert_eq!(pte & (PTE_READ | PTE_WRITE | PTE_EXECUTE), 0);
                (pte >> 10) << 12
            } else {
                let page = shadow_page_tables.alloc_page();
                shadow_page_tables.region.set_nonleaf_pte(pte_addr, (page >> 2) | PTE_VALID);
                page
            };
            shadow_page_tables.region.set_leaf_pte(page_table + ((va >> 21) & 0x1ff) * 8,
                                                   (pa >> 2) | PTE_AD | PTE_USER | PTE_RWXV);
        }

        (shadow_page_tables, guest_memory)
    }
}

#[allow(unused)]
pub fn print_page_table(pt: u64, level: u8) {
    unsafe {
        for i in 0..512 {
            let pte = *(pa2va(pt + i*8) as *const u64);
            if pte & PTE_VALID != 0 {
                for _ in 0..(4 - level) {
                    print!("  ");
                }
                println!("{:#x}: {:#x}", i *8, pte);
            }
            if pte & PTE_RWXV == PTE_VALID {
                assert!(level != 0);
                print_page_table((pte >> 10) << 12, level - 1);
            }
        }
    }
}

#[allow(unused)]
pub fn print_guest_page_table(guest_memory: &MemoryRegion, pt: u64, level: u8, base: u64) {
    if !guest_memory.in_region(pt) {
        println!("[SATP Invalid]");
        return;
    }

    for i in 0..512 {
        let addr = base + (i << (12 + level * 9));
        let pte = guest_memory[pt + i*8];
        if pte == 0 {
            continue;
        }

        for _ in 0..(2 - level) {
            print!("__ ");
        }

        if pte & PTE_RWXV == PTE_VALID {
            assert!(level != 0);
            let child = (pte >> 10) << 12;
            if !guest_memory.in_region(child) {
                println!("{:#x}: {:#x} (bad ppn)", addr, pte);
            } else {
                println!("{:#x}: {:#x}", addr, pte);
                print_guest_page_table(guest_memory, child, level - 1, addr);
                //break;
            }
        } else if pte & PTE_VALID != 0 {
            println!("{:#x} -> {:#x}", addr, (pte >> 10) << 12);
        } else if pte != 0 {
            println!("{:#x}: {:#x} (not valid)", addr, pte);
        }
    }
}

pub fn flush_shadow_page_table(shadow_page_tables: &mut PageTables) {
    for &root in &[UVA, KVA, MVA] {
        shadow_page_tables.clear_page_table_range(shadow_page_tables.root_pa(root), 0, DIRECT_MAP_PT_INDEX/8);
    }

    riscv::sfence_vma();
}

#[inline]
pub fn handle_sfence_vma(state: &mut Context, _instruction: Instruction) {
    flush_shadow_page_table(&mut state.shadow_page_tables);
}

pub fn read64(guest_memory: &MemoryRegion, page_table_ppn: u64, guest_va: u64) -> Option<u64> {
    let guest_page = guest_va & !0xfff;
    if let Some(page_translation) = translate_guest_address(guest_memory, page_table_ppn << 12, guest_page) {
        // assert!(!virtio::is_queue_access(state, page_translation.guest_pa));
        let guest_pa = (page_translation.guest_pa & !0xfff) | (guest_va & 0xfff);
        return guest_memory.get(guest_pa);
    }

    None
}
