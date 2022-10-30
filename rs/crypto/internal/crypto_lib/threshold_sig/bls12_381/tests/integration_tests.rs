#![allow(clippy::unwrap_used)]
//! Tests for combined forward secure encryption and ZK proofs
#![allow(clippy::many_single_char_names)]

use ic_crypto_internal_bls12_381_type::{G1Affine, G1Projective, G2Affine, Scalar};
use ic_crypto_internal_threshold_sig_bls12381::ni_dkg::fs_ni_dkg::{
    forward_secure::*, nizk_chunking::*, nizk_sharing::*,
};
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::Epoch;
use rand::Rng;

#[test]
fn potpourri() {
    let sys = SysParam::global();
    let mut rng = rand::thread_rng();
    const KEY_GEN_ASSOCIATED_DATA: &[u8] = &[2u8, 0u8, 2u8, 1u8];

    println!("generating key pair...");
    let (pk, mut dk) = kgen(KEY_GEN_ASSOCIATED_DATA, sys, &mut rng);
    assert!(
        pk.verify(KEY_GEN_ASSOCIATED_DATA),
        "Forward secure public key failed validation"
    );
    for _i in 0..10 {
        println!("upgrading private key...");
        dk.update(sys, &mut rng);
    }
    let epoch10 = Epoch::from(10);
    let tau10 = tau_from_epoch(sys, epoch10);

    let mut keys = Vec::new();
    for i in 0..=3 {
        println!("generating key pair {}...", i);
        keys.push(kgen(KEY_GEN_ASSOCIATED_DATA, sys, &mut rng));
    }
    let pks = keys
        .iter()
        .map(|key| key.0.key_value.clone())
        .collect::<Vec<_>>();
    let sij: Vec<_> = vec![
        vec![27, 18, 28],
        vec![31415, 8192, 8224],
        vec![99, 999, 9999],
        vec![CHUNK_MIN, CHUNK_MAX, CHUNK_MIN],
    ];
    let associated_data = rng.gen::<[u8; 4]>();
    let (crsz, _toxic) = enc_chunks(&sij, &pks, &tau10, &associated_data, sys, &mut rng).unwrap();

    let dk = &mut keys[1].1;
    for _i in 0..3 {
        println!("upgrading private key...");
        dk.update(sys, &mut rng);
    }

    verify_ciphertext_integrity(&crsz, &tau10, &associated_data, sys)
        .expect("ciphertext integrity check failed");

    let out = dec_chunks(dk, 1, &crsz, &tau10, &associated_data)
        .expect("It should be possible to decrypt");
    println!("decrypted: {:?}", out);
    let mut last3 = vec![0; 3];
    last3[0] = out[13];
    last3[1] = out[14];
    last3[2] = out[15];
    assert!(last3 == sij[1], "decrypt . encrypt == id");

    for _i in 0..8 {
        println!("upgrading private key...");
        dk.update(sys, &mut rng);
    }
    // Should be impossible to decrypt now.
    let out = dec_chunks(dk, 1, &crsz, &tau10, &associated_data);
    match out {
        Err(DecErr::ExpiredKey) => (),
        _ => panic!("old ciphertexts should be lost forever"),
    }
}

/// Tests that the fs proofs of an encrypted chunk validate.
///
/// # Arguments
/// * `epoch` - the epoch for which the data is encrypted.
///
/// Note: This can be extended further by:
/// * Varying the secret key epoch; this is always zero in this test.
/// * Varying the plaintexts more; here we have only fairly noddy variation.
fn encrypted_chunks_should_validate(epoch: Epoch) {
    let sys = SysParam::global();
    let mut rng = rand::thread_rng();
    const KEY_GEN_ASSOCIATED_DATA: &[u8] = &[1u8, 9u8, 8u8, 4u8];

    let num_receivers = 3;
    let threshold = 2;
    let g1 = G1Affine::generator();
    let g2 = G2Affine::generator();

    let receiver_fs_keys: Vec<_> = (0u8..num_receivers)
        .map(|i| {
            println!("generating key pair {}...", i);
            let key_pair = kgen(KEY_GEN_ASSOCIATED_DATA, sys, &mut rng);
            println!("{:#?}", &key_pair.0);
            key_pair
        })
        .collect();
    let public_keys_with_zk: Vec<&PublicKeyWithPop> =
        receiver_fs_keys.iter().map(|key| &key.0).collect();
    // Suggestion: Make the types used by fs encryption and zk proofs consistent.
    // One takes refs, one takes values:
    let receiver_fs_public_keys: Vec<_> = public_keys_with_zk
        .iter()
        .map(|key| key.key_value.clone())
        .collect();

    let polynomial: Vec<_> = (0..threshold).map(|_| Scalar::random(&mut rng)).collect();
    let polynomial_exp: Vec<_> = polynomial
        .iter()
        .map(|term| G2Affine::from(g2 * term))
        .collect();

    // Plaintext, unchunked:
    let plaintexts: Vec<Scalar> = (1..)
        .zip(&receiver_fs_keys)
        .map(|(i, _)| {
            let ibig = Scalar::from_usize(i);
            let mut ipow = Scalar::one();
            let mut acc = Scalar::zero();
            for ak in &polynomial {
                acc += ak * &ipow;
                ipow *= &ibig;
            }
            acc
        })
        .collect();

    // Plaintext, chunked:
    let plaintext_chunks: Vec<Vec<isize>> = plaintexts
        .iter()
        .map(|plaintext| {
            let mut bytes = plaintext.serialize();
            bytes.reverse(); // Make little endian.
            let chunks = bytes[..].chunks(CHUNK_BYTES); // The last, most significant, chunk may be partial.
            chunks
                .map(|chunk| {
                    chunk
                        .iter()
                        .rev()
                        .fold(0, |acc, byte| (acc << 8) + (*byte as isize))
                })
                .rev()
                .collect() // Convert to big endian ints
        })
        .collect();
    println!("Messages: {:#?}", plaintext_chunks);

    // Encrypt
    let tau = tau_from_epoch(sys, epoch);
    let associated_data = rng.gen::<[u8; 10]>();
    let (crsz, encryption_witness) = enc_chunks(
        &plaintext_chunks[..],
        &receiver_fs_public_keys,
        &tau,
        &associated_data,
        sys,
        &mut rng,
    )
    .expect("Encryption failed");

    // Check that decryption succeeds
    let dk = &receiver_fs_keys[1].1;
    let out = dec_chunks(dk, 1, &crsz, &tau, &associated_data);
    println!("decrypted: {:?}", out);
    assert!(
        out.unwrap() == plaintext_chunks[1],
        "decrypt . encrypt == id"
    );

    // chunking proof and verification
    {
        println!("Verifying chunking proof...");
        // Suggestion: Make this conversion in prove_chunking, so that the API types are
        // consistent.
        let big_plaintext_chunks: Vec<Vec<_>> = plaintext_chunks
            .iter()
            .map(|chunks| chunks.iter().copied().map(Scalar::from_isize).collect())
            .collect();

        let chunking_instance = ChunkingInstance::new(
            receiver_fs_public_keys.clone(),
            crsz.cc.clone(),
            crsz.rr.clone(),
        );

        let chunking_witness =
            ChunkingWitness::new(encryption_witness.spec_r.clone(), big_plaintext_chunks);

        let nizk_chunking = prove_chunking(&chunking_instance, &chunking_witness, &mut rng);

        assert_eq!(
            Ok(()),
            verify_chunking(&chunking_instance, &nizk_chunking),
            "verify_chunking verifies NIZK proof"
        );
    }

    // nizk sharing
    {
        println!("Verifying sharing proof...");
        /// Context: Most of this code converts the data used for the fs
        /// encryption to the form needed by the zk crypto. Suggestion:
        /// Put the conversion code in the library.

        /// Combine a big endian array of group elements (first chunk is the
        /// most significant) into a single group element.
        fn g1_from_big_endian_chunks(terms: &[G1Affine]) -> G1Affine {
            let mut acc = G1Projective::identity();

            for term in terms {
                for _ in 0..16 {
                    acc = acc.double();
                }

                acc += term;
            }

            acc.to_affine()
        }

        /// Combine a big endian array of field elements (first chunk is the
        /// most significant) into a single field element.
        fn scalar_from_big_endian_chunks(terms: &[Scalar]) -> Scalar {
            let factor = Scalar::from_u64(1 << 16);

            let mut acc = Scalar::zero();
            for term in terms {
                acc *= &factor;
                acc += term;
            }

            acc
        }

        let combined_ciphertexts: Vec<G1Affine> = crsz
            .cc
            .iter()
            .map(|s| g1_from_big_endian_chunks(s))
            .collect();
        let combined_r = scalar_from_big_endian_chunks(&encryption_witness.spec_r);
        let combined_r_exp = g1_from_big_endian_chunks(&crsz.rr);
        let combined_plaintexts: Vec<Scalar> = plaintext_chunks
            .iter()
            .map(|receiver_chunks| {
                scalar_from_big_endian_chunks(
                    &receiver_chunks
                        .iter()
                        .copied()
                        .map(Scalar::from_isize)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        // Check that the combination is correct:
        // ... for plaintexts:
        for (plaintext, reconstituted_plaintext) in plaintexts.iter().zip(&combined_plaintexts) {
            assert_eq!(
                plaintext, reconstituted_plaintext,
                "Reconstituted plaintext does not match"
            );
        }

        // ... for plaintexts:
        for ((ciphertext, plaintext), public_key) in combined_ciphertexts
            .iter()
            .zip(&plaintexts)
            .zip(&receiver_fs_public_keys)
        {
            let ciphertext_computed_directly =
                G1Projective::mul2(&public_key.into(), &combined_r, &g1.into(), plaintext)
                    .to_affine();
            assert_eq!(
                ciphertext_computed_directly, *ciphertext,
                "Reconstitued ciphertext doesn't match"
            );
        }

        let sharing_instance = SharingInstance::new(
            receiver_fs_public_keys,
            polynomial_exp,
            combined_r_exp,
            combined_ciphertexts,
        );
        let sharing_witness = SharingWitness::new(combined_r, combined_plaintexts);

        let sharing_proof = prove_sharing(&sharing_instance, &sharing_witness, &mut rng);

        assert_eq!(
            Ok(()),
            verify_sharing(&sharing_instance, &sharing_proof),
            "verify_sharing verifies NIZK proof"
        );
    };
}

#[test]
fn encrypted_chunks_should_validate_00() {
    encrypted_chunks_should_validate(Epoch::from(0))
}

#[test]
fn encrypted_chunks_should_validate_01() {
    encrypted_chunks_should_validate(Epoch::from(1))
}

// TODO (CRP-831): Add a test that incorrect encryptions do not validate.
