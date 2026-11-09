//! Export table of a module that is already mapped. RVAs are applied to the
//! loaded base; this is not a parser for files on disk.

#[derive(Clone)]
pub struct Export {
    pub name: String,
    pub addr: *const (),
}

pub struct ExportTable {
    pub code: Vec<Export>,
    pub forwarders: usize,
    pub skipped: usize,
}

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;

struct Section {
    va: u32,
    size: u32,
    exec: bool,
}

/// `base` is a loaded image (the `DllBase` the loader reported).
pub unsafe fn exports(base: *const u8) -> Result<ExportTable, &'static str> {
    unsafe {
        if base.is_null() || u16_at(base, 0)? != 0x5a4d {
            return Err("not a PE");
        }
        let lfanew = u32_at(base, 0x3c)? as usize;
        if lfanew > 0x1000 || u32_at(base, lfanew)? != 0x4550 {
            return Err("not a PE");
        }
        let coff = lfanew + 4;
        let sections_n = u16_at(base, coff + 2)? as usize;
        let opt_size = u16_at(base, coff + 16)? as usize;
        let opt = coff + 20;
        // The data directories start at 96 in PE32 (x86) and 112 in PE32+ (x64).
        let dirs = match u16_at(base, opt)? {
            0x20b => opt + 112,
            0x10b => opt + 96,
            _ => return Err("unknown optional header"),
        };
        let image_size = u32_at(base, opt + 56)? as usize;
        let export_rva = u32_at(base, dirs)?;
        let export_size = u32_at(base, dirs + 4)?;
        if sections_n > 96 {
            return Err("too many sections");
        }
        let sec_off = opt + opt_size;
        let mut sections = Vec::with_capacity(sections_n);
        for i in 0..sections_n {
            let o = sec_off + i * 40;
            let va = u32_at(base, o + 12)?;
            let vsize = u32_at(base, o + 8)?;
            let raw = u32_at(base, o + 16)?;
            let exec = u32_at(base, o + 36)? & SCN_MEM_EXECUTE != 0;
            sections.push(Section {
                va,
                size: vsize.max(raw).max(1),
                exec,
            });
        }
        if export_rva == 0 {
            return Ok(ExportTable {
                code: Vec::new(),
                forwarders: 0,
                skipped: 0,
            });
        }
        // Every table inside the image (one still being laid out by the loader, or a
        // .NET assembly's, can say otherwise).
        let inside = |rva: usize, len: usize| rva.checked_add(len).is_some_and(|end| end <= image_size);
        if !inside(export_rva as usize, 40) {
            return Err("export directory outside the image");
        }
        let dir = base.add(export_rva as usize);
        let n_funcs = u32_at(dir, 20)?;
        let n_names = u32_at(dir, 24)?;
        if n_names > 100_000 || n_funcs > 100_000 {
            return Err("export table too large");
        }
        let (funcs_rva, names_rva, ords_rva) = (u32_at(dir, 28)? as usize, u32_at(dir, 32)? as usize, u32_at(dir, 36)? as usize);
        if !inside(funcs_rva, n_funcs as usize * 4) || !inside(names_rva, n_names as usize * 4) || !inside(ords_rva, n_names as usize * 2) {
            return Err("export tables outside the image");
        }
        let funcs = base.add(funcs_rva);
        let names = base.add(names_rva);
        let ords = base.add(ords_rva);
        let export_end = export_rva.saturating_add(export_size);
        let mut code = Vec::new();
        let mut forwarders = 0usize;
        let mut skipped = 0usize;
        for i in 0..n_names as usize {
            let name_rva = u32_at(names, i * 4)?;
            if name_rva as usize >= image_size {
                skipped += 1;
                continue;
            }
            let name = read_name(base.add(name_rva as usize));
            let ord = u16_at(ords, i * 2)? as u32;
            if ord >= n_funcs {
                skipped += 1;
                continue;
            }
            let func_rva = u32_at(funcs, ord as usize * 4)?;
            if func_rva == 0 {
                skipped += 1;
                continue;
            }
            if func_rva >= export_rva && func_rva < export_end {
                forwarders += 1;
                continue;
            }
            let exec = sections
                .iter()
                .any(|s| func_rva >= s.va && func_rva < s.va.saturating_add(s.size) && s.exec);
            if !exec {
                skipped += 1;
                continue;
            }
            code.push(Export {
                name,
                addr: base.add(func_rva as usize).cast(),
            });
        }
        Ok(ExportTable {
            code,
            forwarders,
            skipped,
        })
    }
}

pub struct Import {
    pub dll: String,
    pub name: String,
    /// The import address table entry the loader filled in.
    pub slot: *mut usize,
}

/// Named imports of a loaded image. Ordinal imports and images without an import name
/// table are skipped, and so are .NET images (a COM descriptor directory): their native
/// imports are only mscoree's entry point, and while .NET loads one (from the loader's
/// load notification) it can still have the 32-bit headers of an AnyCPU assembly or be
/// half laid out, and reading it faulted inside .NET's own exception handling (a .NET
/// apphost program crashed at start under the swap). Every table read stays inside the
/// image's SizeOfImage.
pub unsafe fn imports(base: *const u8) -> Vec<Import> {
    let mut out = Vec::new();
    unsafe {
        let Ok(lfanew) = u32_at(base, 0x3c) else {
            return out;
        };
        let lfanew = lfanew as usize;
        if u16_at(base, 0).ok() != Some(0x5a4d)
            || lfanew > 0x1000
            || u32_at(base, lfanew).ok() != Some(0x4550)
        {
            return out;
        }
        let opt = lfanew + 24;
        let (dirs, wide) = match u16_at(base, opt) {
            Ok(0x20b) => (opt + 112, true),
            Ok(0x10b) => (opt + 96, false),
            _ => return out,
        };
        // The process's own kind of image only (an AnyCPU assembly's PE32 headers in a
        // 64-bit process, or the reverse, aren't native code here).
        if wide != cfg!(target_pointer_width = "64") {
            return out;
        }
        let Ok(size) = u32_at(base, opt + 56).map(|s| s as usize) else {
            return out;
        };
        // IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR (14): a .NET image.
        if u32_at(base, dirs + 14 * 8).is_ok_and(|r| r != 0) {
            return out;
        }
        let Ok(rva) = u32_at(base, dirs + 8) else {
            return out;
        };
        if rva == 0 || rva as usize + 20 > size {
            return out;
        }
        let inside = |rva: usize, len: usize| rva != 0 && rva.checked_add(len).is_some_and(|end| end <= size);
        let entry = if wide { 8 } else { 4 };
        let mut desc_rva = rva as usize;
        for _ in 0..4096 {
            if !inside(desc_rva, 20) {
                break;
            }
            let desc = base.add(desc_rva);
            let (Ok(oft), Ok(name_rva), Ok(ft)) =
                (u32_at(desc, 0), u32_at(desc, 12), u32_at(desc, 16))
            else {
                break;
            };
            if name_rva == 0 {
                break;
            }
            desc_rva += 20;
            if oft == 0 || !inside(name_rva as usize, 1) || !inside(oft as usize, entry) || !inside(ft as usize, entry) {
                continue;
            }
            let dll = read_name(base.add(name_rva as usize));
            for i in 0..65536usize {
                if !inside(oft as usize + i * entry, entry) || !inside(ft as usize + i * entry, entry) {
                    break;
                }
                let thunk = base.add(oft as usize + i * entry);
                let v = if wide {
                    thunk.cast::<u64>().read_unaligned()
                } else {
                    thunk.cast::<u32>().read_unaligned() as u64
                };
                if v == 0 {
                    break;
                }
                let by_ordinal = if wide { v >> 63 != 0 } else { v >> 31 != 0 };
                if by_ordinal || !inside((v as u32) as usize + 2, 1) {
                    continue;
                }
                out.push(Import {
                    dll: dll.clone(),
                    name: read_name(base.add((v as u32) as usize + 2)),
                    slot: base.add(ft as usize + i * entry) as *mut usize,
                });
            }
        }
    }
    out
}

/// Overwrites one import slot, which normally sits in a read-only page.
pub unsafe fn write_slot(slot: *mut usize, value: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    unsafe {
        let mut old = 0u32;
        if VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old) == 0 {
            return false;
        }
        slot.write(value);
        VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut old);
        true
    }
}

unsafe fn read_name(p: *const u8) -> String {
    unsafe {
        let mut out = Vec::new();
        for i in 0..512 {
            let b = p.add(i).read();
            if b == 0 {
                break;
            }
            out.push(b);
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

unsafe fn u16_at(p: *const u8, off: usize) -> Result<u16, &'static str> {
    unsafe { Ok(p.add(off).cast::<u16>().read_unaligned()) }
}

unsafe fn u32_at(p: *const u8, off: usize) -> Result<u32, &'static str> {
    unsafe { Ok(p.add(off).cast::<u32>().read_unaligned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    #[test]
    fn parses_a_loaded_system_dll() {
        let wide: Vec<u16> = OsStr::new("kernel32.dll")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let base = unsafe { GetModuleHandleW(wide.as_ptr()) } as *const u8;
        assert!(!base.is_null());
        let table = unsafe { exports(base).unwrap() };
        assert!(
            table.code.len() > 20,
            "only {} code exports",
            table.code.len()
        );
        assert!(table.code.iter().all(|e| !e.addr.is_null()));
    }

    #[test]
    fn reads_and_patches_an_import_slot() {
        let exe = unsafe { GetModuleHandleW(std::ptr::null()) } as *const u8;
        let imports = unsafe { imports(exe) };
        let imp = imports
            .iter()
            .find(|i| i.dll.eq_ignore_ascii_case("kernel32.dll") && i.name == "GetLastError")
            .or_else(|| {
                imports
                    .iter()
                    .find(|i| i.dll.eq_ignore_ascii_case("kernel32.dll"))
            })
            .expect("test exe imports kernel32");
        let was = unsafe { imp.slot.read() };
        assert_ne!(was, 0);
        assert!(unsafe { write_slot(imp.slot, was) });
        assert_eq!(unsafe { imp.slot.read() }, was);
    }
}
