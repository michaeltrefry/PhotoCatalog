use super::*;
use crate::catalog_session::PhysicalObjectId;
use crate::storage_volume::NativePath;

fn root() -> RootCapability {
    RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(std::path::Path::new("/tmp/export-native")),
        root_physical: PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(2),
        },
        catalog_physical: PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(3),
        },
    }
}

#[test]
fn export_native_key_binds_root_stage_and_operation() -> Result<()> {
    let root = root();
    let stage = LeaseId::new();
    let key = Key::new(&root, U64(7), &stage);
    key.validate()?;
    assert!(key.matches(&root));
    assert_eq!(key.stage, stage);
    let mut zero = key;
    zero.operation = U64(0);
    assert!(zero.validate().is_err());
    Ok(())
}
