## Package System - File-Only Backend Integration Guide

This document explains how to use the new `packagesystem_fileonly` backend as an alternative to the RPM-based package system.

### Overview

**File-Only Backend (`packagesystem_fileonly`)**
- ✅ Distribution-independent (no RPM/package manager required)
- ✅ Uses file metadata: mtime (modification time) + SHA256 prefix
- ✅ Module names are actual filenames (e.g., `shimx64.efi`, `grub2-install`)
- ✅ Version format: `<timestamp>-<sha256_first_12_chars>`
- ✅ Offline operation only
- ✅ Fast (no DB queries, just file stat + hash)
- ✅ No binary signature verification

### Module Structure

**File-Only Module:**
```rust
pub struct Module {
    pub name: String,      // e.g., "shimx64.efi"
    pub rpm_evr: String,   // e.g., "1681321788-4f2a3b5c1a92"
}
```

**Version Example:**
```
When querying files [grub2-install, shimx64.efi]:
version: "grub2-install-1681321788-4f2a3b,shimx64.efi-1682500000-a1b2c3d4e5f6"
```

### Integration Points

#### For Bootloader Components (bios.rs, efi.rs)

Instead of:
```rust
use crate::packagesystem;
let meta = packagesystem::query_files(sysroot_path, [&grub_install])?;
```

Use file-only backend:
```rust
use crate::packagesystem_fileonly;
let meta = packagesystem_fileonly::query_files(sysroot_path, [&grub_install])?;
```

#### Comparison Functions

Both backends provide compatible comparison functions:
```rust
// Compare two version strings
let ordering = packagesystem_fileonly::compare_package_versions(v1, v2);
// Compare Module slices directly
let ordering = packagesystem_fileonly::compare_package_slices(&mods_a, &mods_b);
```

### Usage Example

```rust
use crate::packagesystem_fileonly;
use anyhow::Result;

fn get_bootloader_metadata(sysroot: &str, bootloader_path: &str) -> Result<()> {
    // Query files using mtime+sha256 metadata
    let meta = packagesystem_fileonly::query_files(sysroot, [bootloader_path])?;
    
    println!("Version: {}", meta.version);
    println!("Timestamp: {}", meta.timestamp);
    
    // Compare against current installed version
    if meta.versions.is_some() {
        for module in meta.versions.unwrap() {
            println!("Module: {}-{}", module.name, module.rpm_evr);
        }
    }
    
    Ok(())
}
```

### Feature Flag

A `package-system-fileonly` feature is available in `Cargo.toml` for future conditional compilation:
```toml
[features]
package-system-fileonly = []
```

This can be used to build binaries with file-only as the default backend:
```bash
cargo build --features package-system-fileonly
```

### Advantages Over RPM Backend

| Aspect | RPM Backend | File-Only Backend |
|--------|------------|-------------------|
| Dependencies | Requires rpm tools | None (uses stdlib) |
| Distro Support | RPM-based only | All distros |
| Performance | DB query required | Instant (stat + hash) |
| Portability | Linux-specific RPM DBs | Pure file-based |
| Updatability | Needs package managers | File replacement only |
| Boot Environment | Requires sysroot prep | Works anywhere |

### Limitations

- No package signature verification
- File deletion/modification detected, but not prevented
- Cannot query package relationships or dependencies
- Name mapping must be manual (per bootloader implementation)

### Migration Path

1. Audit which components can switch to file-only (BIOS, EFI modules)
2. Update imports in `bios.rs` and `efi.rs` to use `packagesystem_fileonly`
3. Test version comparison logic remains compatible
4. Consider enabling feature flag for production builds

### Testing

Run tests for the file-only backend:
```bash
cargo test packagesystem_fileonly
```

All tests use temporary files via `tempfile` crate, no external dependencies needed.
