pub mod alloc;
pub mod allocator_bump;
pub mod allocator_system;
pub mod bpf_verifier;
pub mod helpers;

#[macro_export]
macro_rules! solana_bpf_loader {
    () => {
        (
            "solana_bpf_loader".to_string(),
            solana_sdk::bpf_loader::id(),
        )
    };
}

use alloc::Alloc;
use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use log::*;
use solana_rbpf::{EbpfVmRaw, MemoryRegion};
use solana_sdk::account::KeyedAccount;
use solana_sdk::instruction::InstructionError;
use solana_sdk::loader_instruction::LoaderInstruction;
use solana_sdk::pubkey::Pubkey;
use std::io::prelude::*;
use std::io::Error;
use std::mem;

pub fn create_vm(prog: &[u8]) -> Result<(EbpfVmRaw, MemoryRegion), Error> {
    let mut vm = EbpfVmRaw::new(None)?;
    vm.set_verifier(bpf_verifier::check)?;
    vm.set_max_instruction_count(36000)?;
    vm.set_elf(&prog)?;

    let heap_region = helpers::register_helpers(&mut vm)?;

    Ok((vm, heap_region))
}

fn serialize_parameters(
    program_id: &Pubkey,
    keyed_accounts: &mut [KeyedAccount],
    data: &[u8],
) -> Vec<u8> {
    assert_eq!(32, mem::size_of::<Pubkey>());

    let mut v: Vec<u8> = Vec::new();
    v.write_u64::<LittleEndian>(keyed_accounts.len() as u64)
        .unwrap();
    for info in keyed_accounts.iter_mut() {
        v.write_u64::<LittleEndian>(info.signer_key().is_some() as u64)
            .unwrap();
        v.write_all(info.unsigned_key().as_ref()).unwrap();
        v.write_u64::<LittleEndian>(info.account.lamports).unwrap();
        v.write_u64::<LittleEndian>(info.account.data.len() as u64)
            .unwrap();
        v.write_all(&info.account.data).unwrap();
        v.write_all(info.account.owner.as_ref()).unwrap();
    }
    v.write_u64::<LittleEndian>(data.len() as u64).unwrap();
    v.write_all(data).unwrap();
    v.write_all(program_id.as_ref()).unwrap();
    v
}

fn deserialize_parameters(keyed_accounts: &mut [KeyedAccount], buffer: &[u8]) {
    assert_eq!(32, mem::size_of::<Pubkey>());

    let mut start = mem::size_of::<u64>();
    for info in keyed_accounts.iter_mut() {
        start += mem::size_of::<u64>(); // skip signer_key boolean
        start += mem::size_of::<Pubkey>(); // skip pubkey
        info.account.lamports = LittleEndian::read_u64(&buffer[start..]);

        start += mem::size_of::<u64>() // skip lamports
                  + mem::size_of::<u64>(); // skip length tag
        let end = start + info.account.data.len();
        info.account.data.clone_from_slice(&buffer[start..end]);

        start += info.account.data.len() // skip data
                  + mem::size_of::<Pubkey>(); // skip owner
    }
}

pub fn process_instruction(
    program_id: &Pubkey,
    keyed_accounts: &mut [KeyedAccount],
    ix_data: &[u8],
) -> Result<(), InstructionError> {
    solana_logger::setup();

    if let Ok(instruction) = bincode::deserialize(ix_data) {
        match instruction {
            LoaderInstruction::Write { offset, bytes } => {
                if keyed_accounts[0].signer_key().is_none() {
                    warn!("key[0] did not sign the transaction");
                    return Err(InstructionError::GenericError);
                }
                let offset = offset as usize;
                let len = bytes.len();
                debug!("Write: offset={} length={}", offset, len);
                if keyed_accounts[0].account.data.len() < offset + len {
                    warn!(
                        "Write overflow: {} < {}",
                        keyed_accounts[0].account.data.len(),
                        offset + len
                    );
                    return Err(InstructionError::GenericError);
                }
                keyed_accounts[0].account.data[offset..offset + len].copy_from_slice(&bytes);
            }
            LoaderInstruction::Finalize => {
                if keyed_accounts[0].signer_key().is_none() {
                    warn!("key[0] did not sign the transaction");
                    return Err(InstructionError::GenericError);
                }
                keyed_accounts[0].account.executable = true;
                info!(
                    "Finalize: account {:?}",
                    keyed_accounts[0].signer_key().unwrap()
                );
            }
            LoaderInstruction::InvokeMain { data } => {
                if !keyed_accounts[0].account.executable {
                    warn!("BPF account not executable");
                    return Err(InstructionError::GenericError);
                }
                let (progs, params) = keyed_accounts.split_at_mut(1);
                let prog = &progs[0].account.data;
                info!("Call BPF program");
                let (mut vm, heap_region) = match create_vm(prog) {
                    Ok(info) => info,
                    Err(e) => {
                        warn!("Failed to create BPF VM: {}", e);
                        return Err(InstructionError::GenericError);
                    }
                };
                let mut v = serialize_parameters(program_id, params, &data);

                match vm.execute_program(v.as_mut_slice(), &[], &[heap_region]) {
                    Ok(status) => {
                        if 0 == status {
                            warn!("BPF program failed: {}", status);
                            return Err(InstructionError::GenericError);
                        }
                    }
                    Err(e) => {
                        warn!("BPF VM failed to run program: {}", e);
                        return Err(InstructionError::GenericError);
                    }
                }
                deserialize_parameters(params, &v);
                info!(
                    "BPF program executed {} instructions",
                    vm.get_last_instruction_count()
                );
            }
        }
    } else {
        warn!("Invalid instruction data: {:?}", ix_data);
        return Err(InstructionError::GenericError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "Error: Execution exceeded maximum number of instructions")]
    fn test_non_terminating_program() {
        #[rustfmt::skip]
        let prog = &[
            0x07, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // r6 + 1
            0x05, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x00, // goto -2
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
        ];
        let input = &mut [0x00];

        let mut vm = EbpfVmRaw::new(None).unwrap();
        vm.set_verifier(bpf_verifier::check).unwrap();
        vm.set_max_instruction_count(10).unwrap();
        vm.set_program(prog).unwrap();
        vm.execute_program(input, &[], &[]).unwrap();
    }
}
