//! Packed spawn layout for the qualified 25F80 arm64 profile.
//! Decoding owns every byte; no caller pointer is forwarded to the kernel.
use crate::unsupported;
use umbra_core::Result;

pub(crate) const DESCRIPTOR_BYTES: usize = 144;
pub(crate) const ATTRIBUTE_BYTES: usize = 248;
pub(crate) const ACTION_STRIDE: usize = 1040;
pub(crate) const MAX_ACTIONS: usize = 64;
pub(crate) const REGION_BYTES: usize = 81920;
pub(crate) const ACTION_OFFSET: usize = 416;

/// Every component must be queried successfully. A missing observation cannot
/// become a wildcard, including on hosted CI (which tests synthetic profiles).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    pub architecture: String,
    pub product_version: String,
    pub build: String,
    pub release: String,
    pub kernel: String,
    pub image_uuid: [u8; 16],
}

impl Profile {
    pub(crate) fn qualified() -> Self {
        Self {
            architecture: "arm64".into(),
            product_version: "26.5.1".into(),
            build: "25F80".into(),
            release: "25.5.0".into(),
            kernel: "Darwin Kernel Version 25.5.0: Mon Apr 27 20:41:26 PDT 2026; root:xnu-12377.121.6~2/RELEASE_ARM64_T8132".into(),
            image_uuid: [
                0xcc, 0x1c, 0xf9, 0x85, 0xbc, 0x65, 0x37, 0x25,
                0x80, 0x9f, 0x4c, 0x1e, 0x36, 0xb8, 0xf4, 0xba,
            ],
        }
    }

    pub(crate) fn admit(observed: Option<&Self>) -> Result<()> {
        if observed != Some(&Self::qualified()) {
            return Err(unsupported("spawn ABI profile mismatch"));
        }
        Ok(())
    }
}

use umbra_core::SpawnAction as Action;

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub attributes: [u8; ATTRIBUTE_BYTES],
    pub actions: Vec<Action>,
    pub action_bytes: Vec<u8>,
}

impl Snapshot {
    pub(crate) fn require_supported(&self) -> Result<()> {
        for action in &self.actions {
            if let Some(diagnostic) = action.unsupported_diagnostic() {
                return Err(unsupported(diagnostic));
            }
        }
        Ok(())
    }

    /// Build zero-filled, self-contained remote backing after allocation.
    /// Even the unused attribute tail is readable by the qualified kernel.
    pub(crate) fn backing(&self, address: u64) -> Result<Vec<u8>> {
        if address.checked_add(REGION_BYTES as u64).is_none() {
            return Err(malformed());
        }
        let mut bytes = vec![0; REGION_BYTES];
        bytes[..8].copy_from_slice(&(ATTRIBUTE_BYTES as u64).to_le_bytes());
        bytes[8..16].copy_from_slice(&(address + 144).to_le_bytes());
        bytes[144..144 + ATTRIBUTE_BYTES].copy_from_slice(&self.attributes);
        if !self.action_bytes.is_empty() {
            bytes[16..24].copy_from_slice(&(self.action_bytes.len() as u64).to_le_bytes());
            bytes[24..32].copy_from_slice(&(address + ACTION_OFFSET as u64).to_le_bytes());
            bytes[ACTION_OFFSET..ACTION_OFFSET + self.action_bytes.len()]
                .copy_from_slice(&self.action_bytes);
        }
        Ok(bytes)
    }
}

fn malformed() -> umbra_core::UmbraError {
    unsupported("spawn malformed packed descriptor")
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub(crate) fn baseline() -> [u8; ATTRIBUTE_BYTES] {
    let mut bytes = [0; ATTRIBUTE_BYTES];
    bytes[56] = 1;
    bytes[68..80].fill(0xff);
    bytes[112..128].fill(0xff);
    bytes
}

fn padding(offset: usize) -> bool {
    matches!(offset, 2..=3 | 66..=67 | 107 | 169 | 188..=191)
}

/// `read` must either fill the entire requested buffer or return an error.
/// Structural checks precede semantic refusal, regardless of flag order.
pub(crate) fn decode(
    descriptor: &[u8],
    mut read: impl FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<Snapshot> {
    if descriptor.len() != DESCRIPTOR_BYTES {
        return Err(malformed());
    }
    let mut pairs = [(0u64, 0u64); 9];
    for (index, pair) in pairs.iter_mut().enumerate() {
        *pair = (
            u64_at(descriptor, index * 16),
            u64_at(descriptor, index * 16 + 8),
        );
        if (pair.0 == 0) != (pair.1 == 0) || pair.1.checked_add(pair.0).is_none() {
            return Err(malformed());
        }
    }
    let mut attributes = baseline();
    if pairs[0].0 != 0 {
        if pairs[0].0 != ATTRIBUTE_BYTES as u64 {
            return Err(malformed());
        }
        read(pairs[0].1, &mut attributes).map_err(|_| malformed())?;
    }
    let mut action_bytes = Vec::new();
    if pairs[1].0 != 0 {
        let size = usize::try_from(pairs[1].0).map_err(|_| malformed())?;
        if !(8 + ACTION_STRIDE..=8 + MAX_ACTIONS * ACTION_STRIDE).contains(&size) {
            return Err(malformed());
        }
        action_bytes.resize(size, 0);
        read(pairs[1].1, &mut action_bytes).map_err(|_| malformed())?;
        let allocated = i32_at(&action_bytes, 0);
        let count = i32_at(&action_bytes, 4);
        if count <= 0
            || count as usize > MAX_ACTIONS
            || allocated < count
            || size != 8 + count as usize * ACTION_STRIDE
        {
            return Err(malformed());
        }
        for record in action_bytes[8..].as_chunks::<ACTION_STRIDE>().0.iter() {
            let tag = i32_at(record, 0);
            let path = match tag {
                0 => Some(&record[14..]),
                5 => Some(&record[8..]),
                _ => None,
            };
            if path.is_some_and(|bytes| !bytes.contains(&0)) {
                return Err(malformed());
            }
        }
    }
    if pairs[2..].iter().any(|pair| pair.0 != 0) || attributes[192..].iter().any(|byte| *byte != 0)
    {
        return Err(unsupported("spawn unsupported extension"));
    }
    let flags = u16::from_le_bytes(attributes[..2].try_into().unwrap());
    for (bit, name) in [
        (0x0040, "SETEXEC"),
        (0x0080, "START_SUSPENDED"),
        (0x0400, "SETSID"),
        (0x0002, "SETPGROUP"),
        (0x0001, "RESETIDS"),
    ] {
        if flags & bit != 0 {
            return Err(unsupported(format!("spawn unsupported attribute {name}")));
        }
    }
    let unknown = flags & !(0x0004 | 0x0008 | 0x4000);
    if unknown != 0 {
        return Err(unsupported(format!(
            "spawn unsupported attribute {:#06x}",
            1u16 << unknown.trailing_zeros()
        )));
    }
    let defaults = baseline();
    for offset in 16..ATTRIBUTE_BYTES {
        if !padding(offset) && attributes[offset] != defaults[offset] {
            return Err(unsupported(format!(
                "spawn unsupported attribute field {offset}"
            )));
        }
    }
    for (offset, byte) in attributes.iter_mut().enumerate() {
        if padding(offset) {
            *byte = 0;
        }
    }
    attributes[..2].copy_from_slice(&(flags | 0x0080).to_le_bytes());
    let mut actions = Vec::new();
    if !action_bytes.is_empty() {
        for record in action_bytes[8..]
            .as_chunks_mut::<ACTION_STRIDE>()
            .0
            .iter_mut()
        {
            let tag = i32_at(record, 0);
            let fd = i32_at(record, 4);
            actions.push(match tag {
                1 => Action::Close(fd),
                2 => Action::Dup2(fd, i32_at(record, 8)),
                3 => Action::Inherit(fd),
                other => Action::Unsupported(other),
            });
            // Do not expose unused union backing or allocator slop.
            record[if tag == 2 { 12 } else { 8 }..].fill(0);
        }
        let count = actions.len() as i32;
        action_bytes[..4].copy_from_slice(&count.to_le_bytes());
    }
    Ok(Snapshot {
        attributes,
        actions,
        action_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attributes(bytes: &[u8; ATTRIBUTE_BYTES]) -> Result<Snapshot> {
        let mut descriptor = [0; DESCRIPTOR_BYTES];
        descriptor[..8].copy_from_slice(&(ATTRIBUTE_BYTES as u64).to_le_bytes());
        descriptor[8..16].copy_from_slice(&4096u64.to_le_bytes());
        decode(&descriptor, |address, output| {
            assert_eq!(address, 4096);
            output.copy_from_slice(bytes);
            Ok(())
        })
    }

    #[test]
    fn unsupported_semantic_bytes_refuse_and_padding_is_normalized() {
        for offset in 16..ATTRIBUTE_BYTES {
            let mut bytes = baseline();
            bytes[offset] ^= 1;
            let result = attributes(&bytes);
            if padding(offset) {
                assert_eq!(result.unwrap().attributes[offset], 0);
            } else {
                assert!(result.is_err(), "semantic byte {offset}");
            }
        }
    }

    #[test]
    fn signal_fields_are_preserved_and_internal_suspension_is_added() {
        let mut bytes = baseline();
        bytes[..2].copy_from_slice(&0x400cu16.to_le_bytes());
        bytes[4..16].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let decoded = attributes(&bytes).unwrap();
        assert_eq!(&decoded.attributes[4..16], &bytes[4..16]);
        assert_eq!(&decoded.attributes[..2], &0x408cu16.to_le_bytes());
    }

    #[test]
    fn setsid_wins_over_setpgroup_and_malformed_wins_over_flags() {
        let mut bytes = baseline();
        bytes[..2].copy_from_slice(&0x0402u16.to_le_bytes());
        assert!(attributes(&bytes)
            .unwrap_err()
            .to_string()
            .contains("attribute SETSID"));
        let mut descriptor = [0; DESCRIPTOR_BYTES];
        descriptor[..8].copy_from_slice(&247u64.to_le_bytes());
        descriptor[8..16].copy_from_slice(&4096u64.to_le_bytes());
        assert!(decode(&descriptor, |_, _| panic!("must not read"))
            .unwrap_err()
            .to_string()
            .contains("malformed packed descriptor"));
    }

    #[test]
    fn profile_requires_every_identity_and_successful_queries() {
        let expected = Profile::qualified();
        Profile::admit(Some(&expected)).unwrap();
        assert!(Profile::admit(None).is_err());
        for field in 0..6 {
            let mut observed = expected.clone();
            match field {
                0 => observed.architecture.push('?'),
                1 => observed.product_version.push('?'),
                2 => observed.build.push('?'),
                3 => observed.release.push('?'),
                4 => observed.kernel.push('?'),
                5 => observed.image_uuid[0] ^= 1,
                _ => unreachable!(),
            }
            assert!(
                Profile::admit(Some(&observed))
                    .unwrap_err()
                    .to_string()
                    .contains("spawn ABI profile mismatch"),
                "field {field}"
            );
        }
    }

    fn action_plan(records: &[(i32, i32, i32)]) -> Vec<u8> {
        let mut bytes = vec![0; 8 + records.len() * ACTION_STRIDE];
        bytes[..4].copy_from_slice(&(records.len() as i32).to_le_bytes());
        bytes[4..8].copy_from_slice(&(records.len() as i32).to_le_bytes());
        for (record, (tag, source, destination)) in bytes[8..]
            .as_chunks_mut::<ACTION_STRIDE>()
            .0
            .iter_mut()
            .zip(records)
        {
            record[..4].copy_from_slice(&tag.to_le_bytes());
            record[4..8].copy_from_slice(&source.to_le_bytes());
            record[8..12].copy_from_slice(&destination.to_le_bytes());
        }
        bytes
    }

    fn actions(bytes: &[u8]) -> Result<Snapshot> {
        let mut descriptor = [0; DESCRIPTOR_BYTES];
        descriptor[16..24].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        descriptor[24..32].copy_from_slice(&8192u64.to_le_bytes());
        decode(&descriptor, |address, output| {
            assert_eq!(address, 8192);
            output.copy_from_slice(bytes);
            Ok(())
        })
    }

    #[test]
    fn ordered_kernel_actions_preserve_operands_and_clear_union_slop() {
        let mut bytes = action_plan(&[(1, 3, 0), (2, 4, 3), (3, 3, 0), (2, 7, 7)]);
        for record in bytes[8..].as_chunks_mut::<ACTION_STRIDE>().0.iter_mut() {
            record[12..].fill(0xaa);
        }
        let snapshot = actions(&bytes).unwrap();
        assert_eq!(
            snapshot.actions,
            vec![
                Action::Close(3),
                Action::Dup2(4, 3),
                Action::Inherit(3),
                Action::Dup2(7, 7),
            ]
        );
        for record in snapshot.action_bytes[8..]
            .as_chunks::<ACTION_STRIDE>()
            .0
            .iter()
        {
            assert!(record[12..].iter().all(|byte| *byte == 0));
        }
        // Negative fds are kernel error operands, not structural errors.
        assert_eq!(
            actions(&action_plan(&[(2, -1, 3)])).unwrap().actions,
            vec![Action::Dup2(-1, 3)]
        );
    }

    #[test]
    fn malformed_action_counts_and_sizes_refuse_before_semantics() {
        let valid = action_plan(&[(1, 3, 0)]);
        for (offset, value) in [(0, 0i32), (4, 0), (4, -1), (4, 65), (4, 2)] {
            let mut bytes = valid.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(actions(&bytes)
                .unwrap_err()
                .to_string()
                .contains("malformed packed descriptor"));
        }
        let mut truncated = valid.clone();
        truncated.pop();
        assert!(actions(&truncated).is_err());
        assert!(actions(&action_plan(&vec![(1, 3, 0); 64])).is_ok());
        assert!(actions(&action_plan(&vec![(1, 3, 0); 65])).is_err());
        const { assert!(ACTION_OFFSET + 8 + MAX_ACTIONS * ACTION_STRIDE <= REGION_BYTES) };
        const { assert!(144 + ATTRIBUTE_BYTES <= 400) };
        const { assert!(192 <= ATTRIBUTE_BYTES) }; // Kernel prefix has full backing.
    }

    #[test]
    fn late_unsupported_action_refuses_the_complete_plan() {
        for (tag, diagnostic) in [
            (0, "OPEN"),
            (4, "FILEPORT_DUP2"),
            (5, "CHDIR"),
            (6, "FCHDIR"),
            (99, "99"),
        ] {
            let bytes = action_plan(&[(2, 4, 1), (tag, 3, 0)]);
            assert!(actions(&bytes)
                .unwrap()
                .require_supported()
                .unwrap_err()
                .to_string()
                .contains(&format!("spawn unsupported action {diagnostic}")));
        }
    }

    #[test]
    fn inconsistent_pairs_overflow_and_failed_reads_are_malformed() {
        for (size, pointer) in [(248u64, 0u64), (0, 4096), (248, u64::MAX - 4)] {
            let mut descriptor = [0; DESCRIPTOR_BYTES];
            descriptor[..8].copy_from_slice(&size.to_le_bytes());
            descriptor[8..16].copy_from_slice(&pointer.to_le_bytes());
            assert!(decode(&descriptor, |_, _| panic!("invalid pair read"))
                .unwrap_err()
                .to_string()
                .contains("malformed packed descriptor"));
        }
        let mut descriptor = [0; DESCRIPTOR_BYTES];
        descriptor[..8].copy_from_slice(&248u64.to_le_bytes());
        descriptor[8..16].copy_from_slice(&4096u64.to_le_bytes());
        assert!(
            decode(&descriptor, |_, _| Err(crate::error("read", "short")))
                .unwrap_err()
                .to_string()
                .contains("malformed packed descriptor")
        );
    }

    #[test]
    fn extension_pointers_are_refused_without_dereferencing_them() {
        for pair in 2..9 {
            let mut descriptor = [0; DESCRIPTOR_BYTES];
            descriptor[pair * 16..pair * 16 + 8].copy_from_slice(&16u64.to_le_bytes());
            descriptor[pair * 16 + 8..pair * 16 + 16].copy_from_slice(&0x5000u64.to_le_bytes());
            assert!(
                decode(&descriptor, |_, _| panic!("extension must not be read"))
                    .unwrap_err()
                    .to_string()
                    .contains("spawn unsupported extension")
            );
        }
    }

    #[test]
    fn backing_contains_all_kernel_reads_and_only_owned_pointers() {
        let snapshot = actions(&action_plan(&[(2, 4, 1)])).unwrap();
        let base = 0x100000;
        let backing = snapshot.backing(base).unwrap();
        assert_eq!(backing.len(), REGION_BYTES);
        assert_eq!(u64_at(&backing, 0), ATTRIBUTE_BYTES as u64);
        assert_eq!(u64_at(&backing, 8), base + 144);
        assert_eq!(u64_at(&backing, 24), base + ACTION_OFFSET as u64);
        assert_eq!(&backing[144..392], &snapshot.attributes);
        assert!(backing[32..144].iter().all(|byte| *byte == 0));
        assert!(backing[392..416].iter().all(|byte| *byte == 0));
        assert!(snapshot
            .backing(u64::MAX - REGION_BYTES as u64 + 1)
            .is_err());
    }
}
