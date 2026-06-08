use aes::Aes128;
use cipher::block_padding::Pkcs7;
use cipher::{BlockEncryptMut, KeyInit, KeyIvInit};
use haki_dl::{
    EncryptionMethod, aes_128_cbc_decrypt, aes_128_ecb_decrypt, decrypt_hls_segment_bytes,
    decrypt_hls_segment_file, read_mp4_protection_info, redact_secrets,
};
use std::error::Error;

mod support;
use support::TempDirectory;

#[test]
fn aes_cbc_ecb_and_sample_ctr_decrypt_behaviour() -> Result<(), Box<dyn Error>> {
    let key = [7_u8; 16];
    let iv = [3_u8; 16];
    let plain = b"hello padded world";
    type CbcEnc = cbc::Encryptor<Aes128>;
    type EcbEnc = ecb::Encryptor<Aes128>;
    let cbc_encrypted = CbcEnc::new_from_slices(&key, &iv)
        .map_err(|_| "CBC encryptor setup failed")?
        .encrypt_padded_vec_mut::<Pkcs7>(plain);
    let ecb_encrypted = EcbEnc::new_from_slice(&key)
        .map_err(|_| "ECB encryptor setup failed")?
        .encrypt_padded_vec_mut::<Pkcs7>(plain);

    assert_eq!(aes_128_cbc_decrypt(&cbc_encrypted, &key, &iv)?, plain);
    assert_eq!(aes_128_ecb_decrypt(&ecb_encrypted, &key)?, plain);
    assert_eq!(
        decrypt_hls_segment_bytes(EncryptionMethod::SampleAesCtr, plain, Some(&key), Some(&iv))?,
        plain
    );
    Ok(())
}

#[tokio::test]
async fn decrypt_hls_segment_file_replaces_file_in_place() -> Result<(), Box<dyn Error>> {
    let temp = TempDirectory::new("decrypt-file")?;
    let path = temp.path().join("segment.bin");
    let key = [1_u8; 16];
    let iv = [2_u8; 16];
    let plain = b"segment payload";
    type CbcEnc = cbc::Encryptor<Aes128>;
    let encrypted = CbcEnc::new_from_slices(&key, &iv)
        .map_err(|_| "CBC encryptor setup failed")?
        .encrypt_padded_vec_mut::<Pkcs7>(plain);
    std::fs::write(&path, encrypted)?;

    decrypt_hls_segment_file(&path, EncryptionMethod::Aes128, Some(&key), Some(&iv)).await?;

    assert_eq!(std::fs::read(path)?, plain);
    Ok(())
}

#[test]
fn mp4_init_reader_extracts_scheme_kid_pssh_and_multi_drm() {
    let kid = [0_u8; 16];
    let pssh_payload = {
        let mut payload = vec![0x08, 0x01];
        payload.extend(kid);
        payload
    };
    let init = box_bytes(
        b"moov",
        [
            full_box(
                b"pssh",
                [
                    widevine_system_id().as_slice(),
                    &(pssh_payload.len() as u32).to_be_bytes(),
                    pssh_payload.as_slice(),
                ]
                .concat(),
            ),
            box_bytes(
                b"encv",
                box_bytes(
                    b"sinf",
                    [
                        full_box(b"schm", [b"cenc".as_slice(), &[0, 1, 0, 0]].concat()),
                        box_bytes(
                            b"schi",
                            full_box(b"tenc", [[0, 0, 1, 8].as_slice(), kid.as_slice()].concat()),
                        ),
                    ]
                    .concat(),
                ),
            ),
        ]
        .concat(),
    );

    let info = read_mp4_protection_info(&init);

    assert_eq!(
        info.kid.as_deref(),
        Some("00000000000000000000000000000000")
    );
    assert_eq!(info.scheme.as_deref(), Some("cenc"));
    assert!(info.pssh.is_some());
    assert_eq!(info.pssh_system, Some(haki_dl::PsshSystem::Widevine));
    assert_eq!(info.psshs[0].system, haki_dl::PsshSystem::Widevine);
    assert!(info.is_multi_drm);
}

#[test]
fn mp4_init_reader_labels_playready_pssh_from_system_id() {
    let mut pssh_payload = Vec::new();
    pssh_payload.extend([0, 0, 0, 0]);
    pssh_payload.extend(playready_system_id());
    pssh_payload.extend(3_u32.to_be_bytes());
    pssh_payload.extend([1, 2, 3]);
    let init = box_bytes(
        b"moov",
        [box_bytes(b"pssh", pssh_payload).as_slice()].concat(),
    );

    let info = read_mp4_protection_info(&init);

    assert_eq!(info.pssh.as_deref(), Some("AQID"));
    assert_eq!(info.pssh_system, Some(haki_dl::PsshSystem::PlayReady));
    assert_eq!(info.psshs[0].system, haki_dl::PsshSystem::PlayReady);
}

#[test]
fn mp4_init_reader_labels_fairplay_pssh_from_system_id() {
    let mut pssh_payload = Vec::new();
    pssh_payload.extend([0, 0, 0, 0]);
    pssh_payload.extend(fairplay_system_id());
    pssh_payload.extend(3_u32.to_be_bytes());
    pssh_payload.extend([1, 2, 3]);
    let init = box_bytes(
        b"moov",
        [box_bytes(b"pssh", pssh_payload).as_slice()].concat(),
    );

    let info = read_mp4_protection_info(&init);

    assert_eq!(info.pssh.as_deref(), Some("AQID"));
    assert_eq!(info.pssh_system, Some(haki_dl::PsshSystem::FairPlay));
    assert_eq!(info.psshs[0].system, haki_dl::PsshSystem::FairPlay);
}

#[test]
fn mp4_init_reader_retains_multiple_known_pssh_boxes() {
    let init = box_bytes(
        b"moov",
        [
            pssh_box(widevine_system_id(), &[1, 2, 3]).as_slice(),
            pssh_box(playready_system_id(), &[4, 5, 6]).as_slice(),
            pssh_box(fairplay_system_id(), &[7, 8, 9]).as_slice(),
        ]
        .concat(),
    );

    let info = read_mp4_protection_info(&init);

    assert_eq!(info.psshs.len(), 3);
    assert_eq!(info.psshs[0].system, haki_dl::PsshSystem::Widevine);
    assert_eq!(info.psshs[1].system, haki_dl::PsshSystem::PlayReady);
    assert_eq!(info.psshs[2].system, haki_dl::PsshSystem::FairPlay);
}

#[test]
fn redaction_covers_keys_and_signed_url_values() {
    let value = "https://cdn.example/seg.m4s?token=abc&sig=def --key 001122:keyvalue key=secret decryption_key abc";
    let redacted = redact_secrets(value);

    assert!(!redacted.contains("secret"));
    assert!(!redacted.contains("keyvalue"));
    assert!(!redacted.contains("token=abc"));
    assert!(!redacted.contains("sig=def"));
}

fn box_bytes(name: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend(((payload.len() + 8) as u32).to_be_bytes());
    output.extend(name);
    output.extend(payload);
    output
}

fn full_box(name: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    box_bytes(name, [[0_u8; 4].as_slice(), payload.as_slice()].concat())
}

fn widevine_system_id() -> Vec<u8> {
    vec![
        0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21,
        0xed,
    ]
}

fn pssh_box(system_id: Vec<u8>, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend([0, 0, 0, 0]);
    payload.extend(system_id);
    payload.extend((data.len() as u32).to_be_bytes());
    payload.extend(data);
    box_bytes(b"pssh", payload)
}

fn playready_system_id() -> Vec<u8> {
    vec![
        0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f,
        0x95,
    ]
}

fn fairplay_system_id() -> Vec<u8> {
    vec![
        0x94, 0xce, 0x86, 0xfb, 0x07, 0xff, 0x4f, 0x43, 0xad, 0xb8, 0x93, 0xd2, 0xfa, 0x96, 0x8c,
        0xa2,
    ]
}
