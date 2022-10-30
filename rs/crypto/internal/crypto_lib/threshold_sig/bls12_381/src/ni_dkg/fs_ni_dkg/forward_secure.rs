#![allow(clippy::needless_range_loop)]

//! Methods for forward secure encryption

// NOTE: the paper uses multiplicative notation for operations on GT,
// while our BLS12-381 API uses additive naming convention, hence
//    u*v  corresponds to u + v
// and
//    g^x  corresponds to g * x

use crate::ni_dkg::fs_ni_dkg::dlog_recovery::{
    CheatingDealerDlogSolver, HonestDealerDlogLookupTable,
};
use crate::ni_dkg::fs_ni_dkg::encryption_key_pop::{
    prove_pop, verify_pop, EncryptionKeyInstance, EncryptionKeyPop,
};
use crate::ni_dkg::fs_ni_dkg::random_oracles::{random_oracle, HashedMap};

use ic_crypto_internal_bls12_381_type::{
    G1Affine, G1Projective, G2Affine, G2Prepared, G2Projective, Gt, Scalar,
};
pub use ic_crypto_internal_types::curves::bls12_381::{G1 as G1Bytes, G2 as G2Bytes};
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::ni_dkg_groth20_bls12_381::FsEncryptionCiphertextBytes;
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::Epoch;
use lazy_static::lazy_static;
use rand::{CryptoRng, RngCore};
use std::collections::LinkedList;
use zeroize::Zeroize;

/// The ciphertext is an element of Fr which is 256-bits
pub(crate) const MESSAGE_BYTES: usize = 32;

/// The size in bytes of a chunk
pub const CHUNK_BYTES: usize = 2;

/// The maximum value of a chunk
pub const CHUNK_SIZE: isize = 1 << (CHUNK_BYTES << 3); // Number of distinct chunks

/// The minimum range of a chunk
pub const CHUNK_MIN: isize = 0;

/// The maximum range of a chunk
pub const CHUNK_MAX: isize = CHUNK_MIN + CHUNK_SIZE - 1;

/// NUM_CHUNKS is simply the number of chunks needed to hold a message (element
/// of Fr)
pub const NUM_CHUNKS: usize = (MESSAGE_BYTES + CHUNK_BYTES - 1) / CHUNK_BYTES;

const DOMAIN_CIPHERTEXT_NODE: &str = "ic-fs-encryption/binary-tree-node";

/// Type for a single bit
#[derive(Copy, Clone, Debug, Eq, PartialEq, Zeroize)]
pub enum Bit {
    Zero = 0,
    One = 1,
}

impl From<u8> for Bit {
    fn from(i: u8) -> Self {
        if i == 0 {
            Bit::Zero
        } else {
            Bit::One
        }
    }
}

impl From<&Bit> for u8 {
    fn from(b: &Bit) -> u8 {
        match &b {
            Bit::Zero => 0,
            Bit::One => 1,
        }
    }
}

impl From<&Bit> for i32 {
    fn from(b: &Bit) -> i32 {
        match &b {
            Bit::Zero => 0,
            Bit::One => 1,
        }
    }
}

/// Generates tau (a vector of bits) from an epoch.
pub fn tau_from_epoch(sys: &SysParam, epoch: Epoch) -> Vec<Bit> {
    (0..sys.lambda_t)
        .rev()
        .map(|index| {
            if (epoch.get() >> index) & 1 == 0 {
                Bit::Zero
            } else {
                Bit::One
            }
        })
        .collect()
}

/// Converts an epoch prefix to an epoch by filling in remaining bits with
/// zeros.
pub fn epoch_from_tau_vec(tau: &[Bit]) -> Epoch {
    let num_bits = ::std::mem::size_of::<Epoch>() * 8;
    Epoch::from(
        (0..num_bits)
            .rev()
            .zip(tau)
            .fold(0u32, |epoch, (shift, tau)| {
                epoch
                    | ((match *tau {
                        Bit::One => 1,
                        Bit::Zero => 0,
                    }) << shift)
            }),
    )
}

/// A node of a Binary Tree Encryption scheme.
///
/// Notation from section 7.2.
pub struct BTENode {
    // Bit-vector, indicating a path in a binary tree.
    pub tau: Vec<Bit>,

    pub a: G1Affine,
    pub b: G2Affine,

    // We split the d's into two groups.
    // The vector `d_h` always contains the last lambda_H points
    // of d_l,...,d_lambda.
    // The list `d_t` contains the other elements. There are at most lambda_T of them.
    // The longer this list, the higher up we are in the binary tree,
    // and the more leaf node keys we are able to derive.
    pub d_t: LinkedList<G2Affine>,
    pub d_h: Vec<G2Affine>,

    pub e: G2Affine,
}

// must implement explicitly as zeroize does not support LinkedList
impl zeroize::Zeroize for BTENode {
    fn zeroize(&mut self) {
        self.tau.zeroize();
        self.a.zeroize();
        self.b.zeroize();
        self.d_t.iter_mut().for_each(|x| x.zeroize());
        self.d_h.zeroize();
        self.e.zeroize();
    }
}

/// A forward-secure secret key is a list of BTE nodes.
///
/// We can derive the keys of any descendant of any node in the list.
/// We obtain forward security by maintaining the list so that we can
/// derive current and future private keys, but none of the past keys.
pub struct SecretKey {
    pub bte_nodes: LinkedList<BTENode>,
}

/// A public key and its associated proof of possession
#[derive(Clone, Debug)]
pub struct PublicKeyWithPop {
    pub key_value: G1Affine,
    pub proof_data: EncryptionKeyPop,
}

impl PublicKeyWithPop {
    pub fn verify(&self, associated_data: &[u8]) -> bool {
        let instance = EncryptionKeyInstance {
            g1_gen: G1Affine::generator().clone(),
            public_key: self.key_value.clone(),
            associated_data: associated_data.to_vec(),
        };
        verify_pop(&instance, &self.proof_data).is_ok()
    }
}

/// NI-DKG system parameters
pub struct SysParam {
    pub lambda_t: usize,
    pub lambda_h: usize,
    pub f0: G2Affine,       // f_0 in the paper.
    pub f: Vec<G2Affine>,   // f_1, ..., f_{lambda_T} in the paper.
    pub f_h: Vec<G2Affine>, // The remaining lambda_H f_i's in the paper.
    pub h: G2Affine,
    h_prep: G2Prepared,
}

/// Generates a (public key, secret key) pair for of forward-secure
/// public-key encryption scheme.
///
/// # Arguments:
/// * `associated_data`: public information for the Proof of Possession of the
///   key.
/// * `sys`: system parameters for the FS Encryption scheme.
/// * `rng`: seeded pseudo random number generator.
pub fn kgen<R: RngCore + CryptoRng>(
    associated_data: &[u8],
    sys: &SysParam,
    rng: &mut R,
) -> (PublicKeyWithPop, SecretKey) {
    let g1 = G1Affine::generator();
    let g2 = G2Affine::generator();

    // x <- getRandomZp
    // rho <- getRandomZp
    // let y = g1^x
    // let pk = (y, pi_dlog)
    // let dk = (g1^rho, g2^x * f0^rho, f1^rho, ..., f_lambda^rho, h^rho)
    // return (pk, dk)
    let spec_x = Scalar::random(rng);
    let rho = Scalar::random(rng);
    let a = G1Affine::from(g1 * &rho);
    let b = G2Projective::mul2(
        &G2Projective::from(g2),
        &spec_x,
        &G2Projective::from(&sys.f0),
        &rho,
    )
    .to_affine();
    let mut d_t = LinkedList::new();
    for f in sys.f.iter() {
        d_t.push_back(G2Affine::from(f * &rho));
    }
    let mut d_h = Vec::new();
    for h in sys.f_h.iter() {
        d_h.push(G2Affine::from(h * &rho));
    }
    let e = G2Affine::from(&sys.h * &rho);
    let bte_root = BTENode {
        tau: Vec::new(),
        a,
        b,
        d_t,
        d_h,
        e,
    };
    let sk = SecretKey::new(bte_root);

    let y = G1Affine::from(g1 * &spec_x);

    let pop_instance = EncryptionKeyInstance {
        g1_gen: G1Affine::generator().clone(),
        public_key: y.clone(),
        associated_data: associated_data.to_vec(),
    };

    let pop =
        prove_pop(&pop_instance, &spec_x, rng).expect("Implementation bug: Pop generation failed");

    (
        PublicKeyWithPop {
            key_value: y,
            proof_data: pop,
        },
        sk,
    )
}

impl SecretKey {
    /// The current key (the end of list of BTENodes) of a `SecretKey` should
    /// always correspond to an epoch described by lambda_t bits. Some
    /// internal operations break this invariant, leaving less than lambda_t
    /// bits in the current key. This function should be called when this
    /// happens; it modifies the list so the current key corresponds to the
    /// first epoch of the subtree described by the current key.
    ///
    /// For example, if lambda_t = 5, then [..., 011] will change to
    /// [..., 0111, 01101, 01100].
    /// The current key's `tau` now has 5 bits, and the other entries cover the
    /// rest of the 011 subtree after we delete the current key.
    ///
    /// Another example: during the very first epoch the private key is
    /// [1, 01, 001, 0001, 00001, 00000].
    ///
    /// This makes key update easy: pop off the current key, then call this
    /// function.
    ///
    /// An alternative is to only store the root nodes of the subtrees that
    /// cover the remaining valid keys. Thus the first epoch, the private
    /// key would simply be \[0\], and would only change to [1, 01, 001, 0001,
    /// 00001] after the first update. Generally, some computations
    /// happen one epoch later than they would with our current scheme. However,
    /// key update is a bit fiddlier.
    ///
    /// No-op if `self` is empty.
    pub(crate) fn fast_derive<R: RngCore + CryptoRng>(&mut self, sys: &SysParam, rng: &mut R) {
        let mut epoch = Vec::new();
        if self.bte_nodes.is_empty() {
            return;
        }
        let now = self.current().expect("bte_nodes unexpectedly empty");
        for i in 0..sys.lambda_t {
            if i < now.tau.len() {
                epoch.push(now.tau[i]);
            } else {
                epoch.push(Bit::Zero);
            }
        }
        self.update_to(&epoch, sys, rng);
    }

    fn new(bte_root: BTENode) -> SecretKey {
        let mut bte_nodes = LinkedList::new();
        bte_nodes.push_back(bte_root);
        SecretKey { bte_nodes }
    }

    /// Returns this key's  BTE-node that corresponds to the current epoch.
    pub fn current(&self) -> Option<&BTENode> {
        self.bte_nodes.back()
    }

    /// Updates this key to the next epoch.  After an update,
    /// the decryption keys for previous epochs are not accessible any more.
    /// (KUpd(dk, 1) from Sect. 9.1)
    pub fn update<R: RngCore + CryptoRng>(&mut self, sys: &SysParam, rng: &mut R) {
        self.fast_derive(sys, rng);
        match self.bte_nodes.pop_back() {
            None => {}
            Some(mut dk) => {
                dk.zeroize();
                self.fast_derive(sys, rng);
            }
        }
    }

    /// Updates `self` to the given `epoch`.
    ///
    /// If `epoch` is in the past, then disables `self`.
    pub fn update_to<R: RngCore + CryptoRng>(
        &mut self,
        epoch: &[Bit],
        sys: &SysParam,
        rng: &mut R,
    ) {
        // dropWhileEnd (\node -> not $ tau node `isPrefixOf` epoch) bte_nodes
        loop {
            match self.bte_nodes.back() {
                None => return,
                Some(cur) => {
                    if is_prefix(&cur.tau, epoch) {
                        break;
                    }
                }
            }
            self.bte_nodes
                .pop_back()
                .expect("bte_nodes unexpectedly empty")
                .zeroize();
        }

        let g1 = G1Affine::generator();

        // At this point, bte_nodes.back() is a prefix of `epoch`.
        // Replace it with the nodes for `epoch` and later (in the subtree).
        //
        // For example, with a 5-bit epoch, if `node` is 011, and `epoch` is
        // 01101, then we replace [..., 011] with [..., 0111, 01101]:
        //   * The current epoch is now 01101.
        //   * We can still derive the keys for 01110 and 01111 from 0111.
        //   * We can no longer decrypt 01100.
        let mut node = self.bte_nodes.pop_back().expect("self.bte_nodes was empty");
        let mut n = node.tau.len();
        // Nothing to do if `node.tau` is already `epoch`.
        if n == epoch.len() {
            self.bte_nodes.push_back(node);
            return;
        }
        let mut d_t = node.d_t.clone();
        // Accumulators.
        //   b_acc = b * product [d_i^tau_i | i <- [1..n]]
        //   f_acc = f0 * product [f_i^tau_i | i <- [1..n]]
        let mut b_acc = G2Projective::from(&node.b);
        let mut f_acc = ftau_partial(&node.tau, sys).expect("node.tau not the expected size");
        let mut tau = node.tau.clone();
        while n < epoch.len() {
            if epoch[n] == Bit::Zero {
                // Save the root of the right subtree for later.
                let mut tau_1 = tau.clone();
                tau_1.push(Bit::One);
                let delta = Scalar::random(rng);

                let a_blind = (g1 * &delta) + &node.a;
                let mut b_blind =
                    G2Projective::from(d_t.pop_front().expect("d_t not sufficiently large"));
                b_blind += &b_acc;
                b_blind += (&f_acc + &sys.f[n]) * &delta;

                let e_blind = (&sys.h * &delta) + &node.e;
                let mut d_t_blind = LinkedList::new();
                let mut k = n + 1;
                d_t.iter().for_each(|d| {
                    let tmp = (&sys.f[k] * &delta) + d;
                    d_t_blind.push_back(tmp.to_affine());
                    k += 1;
                });
                let mut d_h_blind = Vec::new();
                node.d_h.iter().zip(&sys.f_h).for_each(|(d, f)| {
                    let tmp = (f * &delta) + d;
                    d_h_blind.push(tmp.to_affine());
                });
                self.bte_nodes.push_back(BTENode {
                    tau: tau_1,
                    a: a_blind.to_affine(),
                    b: b_blind.to_affine(),
                    d_t: d_t_blind,
                    d_h: d_h_blind,
                    e: e_blind.to_affine(),
                });
            } else {
                // Update accumulators.
                f_acc += &sys.f[n];
                b_acc += d_t.pop_front().expect("d_t not sufficiently large");
            }
            tau.push(epoch[n]);
            n += 1;
        }

        let delta = Scalar::random(rng);
        let a = g1 * &delta + &node.a;
        let e = &sys.h * &delta + &node.e;
        b_acc += f_acc * &delta;

        let mut d_t_blind = LinkedList::new();
        // Typically `d_t_blind` remains empty.
        // It is only nontrivial if `epoch` is less than LAMBDA_T bits.
        let mut k = n;
        d_t.iter().for_each(|d| {
            let tmp = (&sys.f[k] * &delta) + d;
            d_t_blind.push_back(tmp.to_affine());
            k += 1;
        });
        let mut d_h_blind = Vec::new();
        node.d_h.iter().zip(&sys.f_h).for_each(|(d, f)| {
            let tmp = f * &delta + d;
            d_h_blind.push(tmp.to_affine());
        });

        self.bte_nodes.push_back(BTENode {
            tau,
            a: a.to_affine(),
            b: b_acc.to_affine(),
            d_t: d_t_blind,
            d_h: d_h_blind,
            e: e.to_affine(),
        });
        node.zeroize();
    }
}

/// Forward secure ciphertexts
///
/// This is (C,R,S,Z) tuple of section 5.2, with multiple C values,
/// one for each recipent.
#[derive(Debug)]
pub struct FsEncryptionCiphertext {
    pub cc: Vec<Vec<G1Affine>>,
    pub rr: Vec<G1Affine>,
    pub ss: Vec<G1Affine>,
    pub zz: Vec<G2Affine>,
}

impl FsEncryptionCiphertext {
    /// Serialises a ciphertext from the internal representation into the standard
    /// form.
    ///
    /// # Panics
    /// This will panic if the internal representation is invalid.  Given that the
    /// internal representation is generated internally, this can happen only if there
    /// is an error in our code.
    pub fn serialize(&self) -> FsEncryptionCiphertextBytes {
        assert_eq!(self.rr.len(), NUM_CHUNKS);
        assert_eq!(self.ss.len(), NUM_CHUNKS);
        assert_eq!(self.zz.len(), NUM_CHUNKS);

        let rand_r = {
            let mut rand_r = [G1Bytes([0u8; G1Bytes::SIZE]); NUM_CHUNKS];
            for (dst, src) in rand_r[..].iter_mut().zip(&self.rr) {
                *dst = src.serialize_to::<G1Bytes>();
            }
            rand_r
        };
        let rand_s = {
            let mut rand_s = [G1Bytes([0u8; G1Bytes::SIZE]); NUM_CHUNKS];
            for (dst, src) in rand_s[..].iter_mut().zip(&self.ss) {
                *dst = src.serialize_to::<G1Bytes>();
            }
            rand_s
        };
        let rand_z = {
            let mut rand_z = [G2Bytes([0u8; G2Bytes::SIZE]); NUM_CHUNKS];
            for (dst, src) in rand_z[..].iter_mut().zip(&self.zz) {
                *dst = src.serialize_to::<G2Bytes>();
            }
            rand_z
        };
        let ciphertext_chunks = self
            .cc
            .iter()
            .map(|cj| {
                assert_eq!(cj.len(), NUM_CHUNKS);

                let mut cc = [G1Bytes([0u8; G1Bytes::SIZE]); NUM_CHUNKS];
                for (dst, src) in cc[..].iter_mut().zip(cj) {
                    *dst = src.serialize_to::<G1Bytes>();
                }
                cc
            })
            .collect();

        FsEncryptionCiphertextBytes {
            rand_r,
            rand_s,
            rand_z,
            ciphertext_chunks,
        }
    }

    /// Parses a ciphertext into the internal representation.
    ///
    /// # Errors
    /// This will return an error if any of the constituent group elements is
    /// invalid.
    pub fn deserialize(ciphertext: &FsEncryptionCiphertextBytes) -> Result<Self, &'static str> {
        let rr = G1Affine::batch_deserialize(&ciphertext.rand_r).or(Err("Malformed rand_r"))?;
        let ss = G1Affine::batch_deserialize(&ciphertext.rand_s).or(Err("Malformed rand_s"))?;
        let zz = G2Affine::batch_deserialize(&ciphertext.rand_z).or(Err("Malformed rand_z"))?;

        let cc: Vec<Vec<G1Affine>> = ciphertext
            .ciphertext_chunks
            .iter()
            .map(|cj| G1Affine::batch_deserialize(&cj[..]).or(Err("Malformed ciphertext_chunk")))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { cc, rr, ss, zz })
    }
}

/// Randomness needed for NIZK proofs.
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct EncryptionWitness {
    pub spec_r: Vec<Scalar>,
}

/// Encrypt chunks. Returns ciphertext as well as the witness for later use
/// in the NIZK proofs.
pub fn enc_chunks<R: RngCore + CryptoRng>(
    sij: &[Vec<isize>],
    pks: &[G1Affine],
    tau: &[Bit],
    associated_data: &[u8],
    sys: &SysParam,
    rng: &mut R,
) -> Option<(FsEncryptionCiphertext, EncryptionWitness)> {
    if sij.is_empty() || pks.len() != sij.len() {
        return None;
    }

    let receivers = pks.len();
    let chunks = sij[0].len();

    for i in 0..sij.len() {
        if sij[i].len() != chunks {
            return None; // Chunk lengths disagree.
        }
        for x in sij[i].iter() {
            if *x < CHUNK_MIN || *x > CHUNK_MAX {
                return None; // Chunk out of range.
            }
        }
    }

    let g1 = G1Affine::generator();

    // do
    //   spec_r <- replicateM chunks getRandom
    //   s <- replicateM chunks getRandom
    //   let rr = (g1^) <$> spec_r
    //   let ss = (g1^) <$> s
    let s = Scalar::batch_random(rng, chunks);
    let ss = g1.batch_mul(&s);

    let r = Scalar::batch_random(rng, chunks);
    let rr = g1.batch_mul(&r);

    // [[pk^r * g1^s | (r, s) <- zip rs si] | (pk, si) <- zip pks sij]
    let cc = {
        let mut cc: Vec<Vec<G1Affine>> = Vec::with_capacity(pks.len());

        let g1 = G1Projective::from(g1);

        for i in 0..receivers {
            let pk = G1Projective::from(&pks[i]);

            let mut enc_chunks = Vec::with_capacity(chunks);

            for j in 0..chunks {
                let s = Scalar::from_isize(sij[i][j]);
                enc_chunks.push(G1Projective::mul2(&pk, &r[j], &g1, &s).to_affine());
            }

            cc.push(enc_chunks);
        }

        cc
    };

    let extended_tau = extend_tau(&cc, &rr, &ss, tau, associated_data);
    let id = ftau(&extended_tau, sys).expect("extended_tau not the correct size");
    let mut zz = Vec::with_capacity(chunks);

    for j in 0..chunks {
        zz.push(G2Projective::mul2(&id, &r[j], &G2Projective::from(&sys.h), &s[j]).to_affine())
    }

    Some((
        FsEncryptionCiphertext { cc, rr, ss, zz },
        EncryptionWitness { spec_r: r },
    ))
}

fn is_prefix(xs: &[Bit], ys: &[Bit]) -> bool {
    // isPrefix [] _ = True
    // isPrefix _ [] = False
    // isPrefix (x:xt) (y:yt) = x == y && isPrefix xt yt
    if xs.len() > ys.len() {
        return false;
    }
    for i in 0..xs.len() {
        if xs[i] != ys[i] {
            return false;
        }
    }
    true
}

fn find_prefix<'a>(dks: &'a SecretKey, tau: &[Bit]) -> Option<&'a BTENode> {
    for node in dks.bte_nodes.iter() {
        if is_prefix(&node.tau, tau) {
            return Some(node);
        }
    }
    None
}

/// Error while decrypting
#[derive(Debug)]
pub enum DecErr {
    ExpiredKey,
    InvalidChunk,
    InvalidCiphertext,
}

/// Decrypt the i-th group of chunks.
///
/// Decrypting a message for a future epoch hardly costs more than a message for
/// a current epoch: at most lambda_t point additions.
///
/// Upgrading a key is expensive in comparison because we must compute new
/// subtree roots and re-"blind" them (the random deltas of the paper) to hide
/// ciphertexts from future keys. Each re-blinding costs at least lambda_h
/// (which is 256 in our system) point multiplications.
///
/// Caller must ensure i < n, where n = crsz.cc.len().
pub fn dec_chunks(
    dks: &SecretKey,
    i: usize,
    crsz: &FsEncryptionCiphertext,
    tau: &[Bit],
    associated_data: &[u8],
) -> Result<Vec<isize>, DecErr> {
    let spec_n = crsz.cc.len();
    let spec_m = crsz.cc[i].len();

    if crsz.rr.len() != spec_m || crsz.ss.len() != spec_m || crsz.zz.len() != spec_m {
        return Err(DecErr::InvalidCiphertext);
    }

    let extended_tau = extend_tau(&crsz.cc, &crsz.rr, &crsz.ss, tau, associated_data);
    let dk = match find_prefix(dks, tau) {
        None => return Err(DecErr::ExpiredKey),
        Some(node) => node,
    };
    let mut bneg = G2Projective::from(&dk.b);
    let mut l = dk.tau.len();
    for t in dk.d_t.iter() {
        if extended_tau[l] == Bit::One {
            bneg += t;
        }
        l += 1
    }
    for k in 0..LAMBDA_H {
        if extended_tau[LAMBDA_T + k] == Bit::One {
            bneg += &dk.d_h[k];
        }
    }
    bneg = bneg.neg();

    let cj = &crsz.cc[i];

    let bneg = G2Prepared::from(&bneg);
    let eneg = G2Prepared::from(&dk.e.neg());

    // zipWith4 f cj rr ss zz where
    //   f c r s z =
    //     ate(g2, c) * ate(bneg, r) * ate(z, dk_a) * ate(eneg, s)
    let mut powers = Vec::with_capacity(spec_m);

    for i in 0..spec_m {
        let x = Gt::multipairing(&[
            (&cj[i], G2Prepared::generator()),
            (&crsz.rr[i], &bneg),
            (&dk.a, &G2Prepared::from(&crsz.zz[i])),
            (&crsz.ss[i], &eneg),
        ]);

        powers.push(x);
    }

    // Find discrete log of the powers
    let linear_search = HonestDealerDlogLookupTable::new();
    let mut dlogs = linear_search.solve_several(&powers);

    if dlogs.iter().any(|x| x.is_none()) {
        // Cheating dealer case
        let cheating_solver = CheatingDealerDlogSolver::new(spec_n, spec_m);

        for i in 0..dlogs.len() {
            if dlogs[i].is_none() {
                // It may take hours to brute force a cheater's discrete log.
                dlogs[i] = cheating_solver.solve(&powers[i]);
            }
        }
    }

    let chunk_size = Scalar::from_isize(CHUNK_SIZE);
    let mut acc = Scalar::zero();
    for dlog in dlogs.iter() {
        let dlog = match dlog {
            None => panic!("Unsolvable discrete logarithm in NIDKG"),
            Some(solution) => solution.clone(),
        };
        acc *= &chunk_size;
        acc += dlog;
    }
    let fr_bytes = acc.serialize();

    // Break up fr_bytes into a vec of isize, which will be combined again later.
    // It may be better to simply return FrBytes and change enc_chunks() to take
    // FrBytes and have it break it into chunks. This would confine the chunking
    // logic to the DKG, where it belongs.
    // (I tried this for a while, but it seemed to touch a lot of code.)
    let redundant = fr_bytes[..]
        .chunks_exact(CHUNK_BYTES)
        .map(|x| 256 * (x[0] as isize) + (x[1] as isize))
        .collect();
    Ok(redundant)
}

// TODO(IDX-1866)
#[allow(clippy::result_unit_err)]
/// Verify ciphertext integrity
///
/// Part of DVfy of Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
//
/// In addition to verifying the proofs of chunking and sharing,
/// we must also verify ciphertext integrity.
pub fn verify_ciphertext_integrity(
    crsz: &FsEncryptionCiphertext,
    tau: &[Bit],
    associated_data: &[u8],
    sys: &SysParam,
) -> Result<(), ()> {
    let n = if crsz.cc.is_empty() {
        0
    } else {
        crsz.cc[0].len()
    };
    if crsz.rr.len() != n || crsz.ss.len() != n || crsz.zz.len() != n {
        // In theory, this is unreachable fail because deserialization only succeeds
        // when the vectors of a CRSZ have the same length. (In practice, it's
        // surprising how often "unreachable" code is reached!)
        return Err(());
    }

    let extended_tau = extend_tau(&crsz.cc, &crsz.rr, &crsz.ss, tau, associated_data);
    let id = ftau(&extended_tau, sys).expect("extended_tau not the correct size");

    let g1_neg = G1Affine::generator().neg();
    let precomp_id = G2Prepared::from(&id);

    // check for all j:
    //     1 =
    //      e(g1^{-1}, Z_j) *
    //      e(R_j, f_0 \Prod_{i=0}^{\lambda} f_i^{\tau_i}) *
    //      e(S_j,h)
    let checks: Result<(), ()> = crsz
        .rr
        .iter()
        .zip(crsz.ss.iter().zip(crsz.zz.iter()))
        .try_for_each(|(r, (s, z))| {
            let z = G2Prepared::from(z);

            let v = Gt::multipairing(&[(r, &precomp_id), (s, &sys.h_prep), (&g1_neg, &z)]);

            if v.is_identity() {
                Ok(())
            } else {
                Err(())
            }
        });
    checks
}

/// Returns (tau || RO(cc, rr, ss, tau, associated_data)).
///
/// See the description of Deal in Section 7.1.
pub fn extend_tau(
    cc: &[Vec<G1Affine>],
    rr: &[G1Affine],
    ss: &[G1Affine],
    tau: &[Bit],
    associated_data: &[u8],
) -> Vec<Bit> {
    let mut map = HashedMap::new();
    map.insert_hashed("ciphertext-chunks", &cc.to_vec());
    map.insert_hashed("randomizers-r", &rr.to_vec());
    map.insert_hashed("randomizers-s", &ss.to_vec());
    map.insert_hashed("epoch", &(epoch_from_tau_vec(tau).get() as usize));
    map.insert_hashed("associated-data", &associated_data.to_vec());

    let hash = random_oracle(DOMAIN_CIPHERTEXT_NODE, &map);

    let mut extended_tau: Vec<Bit> = tau.to_vec();
    hash.iter().for_each(|byte| {
        for b in 0..8 {
            extended_tau.push(Bit::from((byte >> b) & 1));
        }
    });
    extended_tau
}

/// Computes the function f of the paper.
///
/// The bit vector tau must have length lambda_T + lambda_H.
pub fn ftau(tau: &[Bit], sys: &SysParam) -> Option<G2Projective> {
    if tau.len() != sys.lambda_t + sys.lambda_h {
        return None;
    }
    let mut id = G2Projective::from(&sys.f0);
    for (n, t) in tau.iter().enumerate() {
        if *t == Bit::One {
            if n < sys.lambda_t {
                id += &sys.f[n];
            } else {
                id += &sys.f_h[n - sys.lambda_t];
            }
        }
    }
    Some(id)
}

/// Computes f for bit vectors tau <= lambda_T.
fn ftau_partial(tau: &[Bit], sys: &SysParam) -> Option<G2Projective> {
    if tau.len() > sys.lambda_t {
        return None;
    }
    // id = product $ f0 : [f | (t, f) <- zip tau sys_fs, t == 1]
    let mut id = G2Projective::from(&sys.f0);
    tau.iter().zip(sys.f.iter()).for_each(|(t, f)| {
        if *t == Bit::One {
            id += f;
        }
    });
    Some(id)
}

// An FS key upgrade can take up to 2 * LAMBDA_T * LAMBDA_H point
// multiplications. This is tolerable in practice for LAMBDA_T = 32, but in
// tests, smaller values are preferable.

/// Constant which controls the upper limit of epochs
///
/// Specifically 2**LAMBDA_T NI-DKG epochs cann occur
///
/// See Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
pub const LAMBDA_T: usize = 32;

/// The size of the hash function used during encryption
///
/// See Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
const LAMBDA_H: usize = 256;

lazy_static! {
    static ref SYSTEM_PARAMS: SysParam =
        SysParam::create(b"DFX01-with-BLS12381G2_XMD:SHA-256_SSWU_RO_");
}

impl SysParam {
    /// Create a set of system parameters
    fn create(dst: &[u8]) -> Self {
        let f0 = G2Affine::hash(dst, b"f0");

        let mut f = Vec::with_capacity(LAMBDA_T);
        for i in 0..LAMBDA_T {
            let s = format!("f{}", i + 1);
            f.push(G2Affine::hash(dst, s.as_bytes()));
        }
        let mut f_h = Vec::with_capacity(LAMBDA_H);
        for i in 0..LAMBDA_H {
            let s = format!("f_h{}", i);
            f_h.push(G2Affine::hash(dst, s.as_bytes()));
        }

        let h = G2Affine::hash(dst, b"h");

        let h_prep = G2Prepared::from(&h);

        SysParam {
            lambda_t: LAMBDA_T,
            lambda_h: LAMBDA_H,
            f0,
            f,
            f_h,
            h,
            h_prep,
        }
    }

    /// Return a reference to the global NI-DKG system parameters
    pub fn global() -> &'static Self {
        &SYSTEM_PARAMS
    }
}
