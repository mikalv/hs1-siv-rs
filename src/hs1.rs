// To the extent possible under law, Isis Agora Lovecruft has waived all copyright and related or
// neighboring rights to hs1-siv-rs, using the Creative Commons "CC0" public domain dedication.
// See <http://creativecommons.org/publicdomain/zero/1.0/> for full details.
//
// Authors: Isis Agora Lovecruft <isis@en.ciph.re> 0xA3ADB67A2CDB8B35

//! A reference implementation of [the HS1-SIV authenticated-encryption cipher, version 2](competitions.cr.yp.to/round2/hs1sivv2.pdf).
//!
//! WARNING: ***REFERENCE IMPLEMENTATION*** MEANS ***FOR REFERENCE ONLY***.
//!
//! The author is quite certain that this implementation is not without side channels, e.g. due to
//! modular exponentiation via Rust's pow() method (which uses a naïve add-and-multiply algorithm).
//!
//! ***USE AT YOUR OWN RISK***.
//!
//! ## TODO:
//!
//!    * Get answers to all questions in the code (marked with `XXX_QUESTION`).
//!    * Cleanup all the places marked with `XXX`.
//!    * Vectorise with `std::simd`?

// We use variable and function names from the HS1-SIV paper throughout this implementation, most
// of which do not conform to Rust's standard of using snake case.
#![allow(non_snake_case)]

use std;
use std::iter::repeat;
use std::result::Result;
use std::slice::Chunks;
use std::vec::Vec;

pub use num::bigint::{BigInt, ToBigInt};
pub use num::traits::{FromPrimitive, ToPrimitive};

use bit_vec::BitVec;

use chacha20::ChaCha20;
use cryptoutil::xor_keystream;
use symmetriccipher::SynchronousStreamCipher; // Used in order to call ChaCha20::process().


macro_rules! u64toBI {
    ($x:expr) => (BigInt::from_u64($x).expect(&format!("Couldn't convert {:?} into BigInt", $x)[..]))
}

#[derive(Debug, Clone, Copy)]
pub enum Error {
    /// Raised upon HS1 decryption if the authenticator cannot be verified.
    AuthenticationError,
    /// Raised if some portion of a `Key` was not formatted correctly.
    KeyFormatError,
}

/// HS1-SIV key material.
///
/// A Key is a vector `(kS, kN, kP, kA)`, where:
///
/// * `kS`, is a string of 32 bytes,
/// * `kN`, is a vector of `b/4 + 4(t-1)` integers from ℤ_2^32,
/// * `kP`, is a vector of `t` integers from ℤ_2^60, and
/// * `kA`, is a vector of `3t` integers from ℤ_2^64,
#[derive(Clone, Eq)]
pub struct Key {
    S: [u8; 32],
    N: Vec<u32>,
    P: Vec<u64>,
    A: Vec<u64>,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "S: {:?},\nN: {:?},\nA: {:?},\nP: {:?}\n", self.S, self.N, self.A, self.P)
    }
}

impl PartialEq for Key {
    #[inline]
    fn eq(&self, other: &Key) -> bool {
        if (self.S != other.S) | (self.N != other.N) | (self.P != other.P) | (self.A != other.A) {
            false
        } else {
            true
        }
    }
}

pub type Ciphertext     = Vec<u8>;
pub type Authenticator  = Vec<u8>;
pub type Plaintext      = Vec<u8>;
pub type AssociatedData = Vec<u8>;

/// Parameters for initialising HS1-SIV with established levels of varying security and efficiency.
///
/// There are four parameters used throughout this specification: `b`, `t`, `r`, and `l`.
///
/// # Pre-defined Parameter Sets
///
/// Most likely, you should only ever need to use one of the following pre-defined sets of
/// parameters:
///
///    * `HS1_SIV_LO` is a pre-defined set of `Parameters` where `b=64`, `t=2`, `r=8`, and `l=8`.
///    * `HS1_SIV` is a pre-defined set of `Parameters` where `b=64`, `t=4`, `r=12`, and `l=16`.
///    * `HS1_SIV_HI` is a pre-defined set of `Parameters` where `b=64`, `t=6`, `r=20`, and `l=32`.
///
/// Parameter `b` specifies the number of bytes used in part of the hashing algorithm (larger `b`
/// tends to produce higher throughput on longer messages).
///
/// Parameter `t` selects the collision level of the hashing algorithm (higher `t` produces higher
/// security and lower throughput).
///
/// Parameter `r` specifies the number of internal rounds used by the stream cipher (higher `r`
/// produces higher security and lower throughput).
///
/// Parameter `l` specifies the byte-length of synthetic IV used (higher `l` improves security and
/// increases ciphertext lengths by `l` bytes).
#[derive(Clone, Copy)]
pub struct                          Parameters{b: u8, t: u8, r: u8, l: u8}
pub static HS1_SIV_LO: Parameters = Parameters{b: 64, t:  2, r:  8, l:  8};
pub static    HS1_SIV: Parameters = Parameters{b: 64, t:  4, r: 12, l: 16};
pub static HS1_SIV_HI: Parameters = Parameters{b: 64, t:  6, r: 20, l: 32};

/// HS1-SIV uses a new PRF called HS1 to provide authenticated encryption via Rogaway and
/// Shrimpton’s SIV mode [6].  HS1 (mnemonic for “Hash-Stream 1”) is designed for high software
/// speed on systems with good 32-bit processing, including Intel SSE, ARM Neon, and 32-bit
/// architectures without SIMD support. HS1 uses a universal hash function to consume arbitrary
/// strings and a stream cipher to produce its output.
///
/// # Features
///
/// HS1-SIV is designed to have the following features.
///
/// ## Competitive speed on multiple architectures.
/// HS1-SIV is designed to exploit 32-bit multiplication and SIMD processing, which are
/// well-supported on almost all current CPUs.  This ensures a consistent performance profile over a
/// wide range of processors, including modern embedded ones.
///
/// ## Provable security.
/// HS1-Hash and SIV are based on well-known and proven constructions. [4] [6]
///
/// The only security assumption needed is that the Chacha stream cipher is a good pseudo-random
/// generator and secure against related-key attacks.
///
/// ## Nonce misuse resistant.
/// No harm is done when a nonce is reused, except that it is revealed whether corresponding
/// (associated data, plaintext) pairs have been repeated. If (associated data, plaintext) pairs are
/// known to never repeat, no nonce need be used at all.
///
/// ## Scalable.
/// Different security levels are available for different tasks, with varying throughput.
///
/// ## General-purpose PRF.
/// The general nature of HS1 makes it useful for a variety of tasks, such as entropy harvesting,
/// random generation, and other IV-based encryption and authentication schemes. A single software
/// library could provide multiple services under a single key by simply partitioning the nonce
/// space and providing access to HS1.  With the exception of provable security, all of the above
/// features are improvements over GCM.
pub struct HS1 {
    /// The parameter set which HS1-SIV was initialised with.
    parameters: Parameters,
}

/// HS1-Subkeygen takes any length key and uses ChaCha to produce all internal keys needed by HS1.
pub trait Subkeygen {
    fn subkeygen(&self, K: &[u8]) -> Key;
}

/// Hash `M` a total of `t` times with different keys and combine the result with the stream
/// cipher’s key.
pub trait PRF {
    fn prf(&self, k: &Key, M: &Vec<u8>, N: &Vec<u8>, y: i64) -> Vec<u8>;
}

/// The hash family HS1-Hash is `(1/2^(28) + l/b2^(60))-AU` for all `M` up to `l` bytes, when
/// `k_N` and `k_P` are chosen randomly and `t ≤ 4`.
pub trait Hash {
    fn hash(&self, kN: &Vec<u32>, kP: &u64, kA: &Vec<u64>, M: &Vec<u8>) -> Vec<u8>;
}

/// Encrypt the message `M` using the HS1-SIV authenticated encryption cipher.
pub trait Encrypt {
    fn encrypt(&self, K: &[u8], M: &Plaintext, A: &AssociatedData, N: &Vec<u8>)
               -> (Ciphertext, Authenticator);
}

/// Decrypt (and authenticate) the ciphertext `C` using the HS1-SIV authenticated encryption cipher.
pub trait Decrypt {
    fn decrypt(&self, K: &[u8], T: &Authenticator, C: &Ciphertext, A: &AssociatedData, N: &Vec<u8>)
               -> Result<Plaintext, Error>;
}

impl HS1 {
    /// Initialise a new HS1 cipher with the chosen `parameters`.
    pub fn new(parameters: Parameters) -> HS1 {
        HS1{ parameters: parameters }
    }
}

/// HS1-Subkeygen takes any length key and uses ChaCha to produce all internal keys needed by HS1.
///
/// # Inputs
///
/// - `K`, a non-empty string of up to 32 bytes
///
/// # Output
///
/// - `k`, a vector `(KS, kN, kP, kA)`, where:
///   - `kS`, is a string of 32 bytes,
///   - `kN`, is a vector of `b/4 + 4(t-1)` integers from ℤ_2^32,
///   - `kP`, is a vector of `t` integers from ℤ_2^60, and
///   - `kA`, is a vector of `3t` integers from ℤ_2^64,
///
/// # Algorithm
///
/// 1. ChachaLen = 32
/// 2. NHLen = b + 16(t-1)
/// 3. PolyLen = 8t
/// 4. ASULen = 24t
/// 5. y = ChachaLen + NHLen + PolyLen + ASULen
/// 6. K' = (K || K || K || K || …)[0, 32]
/// 7. N = toStr(12, b*2^48 + t*2^40 + r*2^32 + l*2^16 + |K|)
/// 8. T = Chacha[r](K', 0, N, 0^y)`, where:
///    - K' is a 32-byte key,
///    - 0 is the initial counter value,
///    - N is a 12-byte IV, and
///    - 0^y is a y-byte string comprised entirely of `0`s, which will be encrypted to
///      produce the new subkey.
/// 9.  kS = T[0, ChachaLen]
/// 10. kN = toInts(4, T[ChachaLen, NHLen])
/// 11. kP = map(mod 2^60, toInts(8, T[ChachaLen, + NHLen, PolyLen]))
/// 12. kA = toInts(8, T[ChachaLen + NHLen + PolyLen, ASULen])
///
/// # Example
//
// We don't run this example because the component attributes of a Key{} are private and can't
// be inspected outside of the test module.
//
/// ```ignored
/// let hs1: HS1 = HS1::new(HS1_SIV_HI);
/// let key: Key = hs1.subkeygen(&([0x01; 32])[..]);
///
/// assert_eq!(key,
///     Key{
///         S: [066, 244, 117, 241, 055, 145, 223, 140,
///             184, 185, 019, 242, 039, 249, 082, 194,
///             023, 201, 191, 186, 009, 175, 018, 002,
///             213, 126, 099, 239, 010, 106, 057, 018],
///         N: vec![3660856796, 2808012582, 0816710194, 1781235828, 1740840331, 0649412028,
///                 2952970402, 3300582805, 2069853897, 2906666258, 2191360017, 2844915405,
///                 1609289759, 0344433639, 1284071806, 2001707277, 0947418435, 1969673357,
///                 0989035411, 0953186313, 3916241572, 1905709989, 0953159336, 2999638165,
///                 0533683549, 1155470835, 3688160385, 2628173954, 3862510477, 2427573020,
///                 1690498549, 0955453051, 0385685213, 2627681714, 3544580233, 1641118666],
///         A: vec![06489449523115878945, 16282780256271739911, 18023248921942667116,
///                 16466514392730658535, 17127315397133553103, 12537775801794843495,
///                 08275771749819178897, 01995321774704745902, 00008549752869278449,
///                 07161945308114179472, 08557883796914475469, 00837391194288851179,
///                 14124367092912090278, 14406494765766506373, 09115428213722762905,
///                 12012076701443349400, 09799299084565746883, 3630251479602537176],
///         P: vec![321925406395680741, 570629164233939075, 242071757283479051,
///                 308576097403662493, 000587647586886788, 147952616260666016],
///        });
/// ```
impl Subkeygen for HS1 {
    fn subkeygen(&self, K: &[u8]) -> Key {
        let chachaLen:  usize = 32;
        let nhLen:      usize = self.parameters.b as usize + 16 * (self.parameters.t as usize - 1);
        let polyLen:    usize =  8 * self.parameters.t as usize;
        let asuLen:     usize = 24 * self.parameters.t as usize;
        let y:          usize = chachaLen + nhLen + polyLen + asuLen;
        let mut chacha: ChaCha20;
        let mut kPrime: [u8; 32];
        let mut N:      Vec<u8>;
        let mut input:  Vec<u8> = repeat(0).take(y).collect();
        let mut output: Vec<u8> = repeat(0).take(y).collect();

        if K.len() >= 32 {
            kPrime = take32(K);
        } else {
            let mut kVec: Vec<u8> = Vec::with_capacity(32);
            while kVec.len() < 32 {
                for byte in K.iter() {
                    kVec.push(*byte);
                }
            }
            kPrime = take32(&kVec[..]);
        }
        assert_eq!(kPrime.len(), 32);

        N = toStr(12, &((self.parameters.b as u64 * 2u64.pow(48) +
                         self.parameters.t as u64 * 2u64.pow(40) +
                         self.parameters.r as u64 * 2u64.pow(32) +
                         self.parameters.l as u64 * 2u64.pow(16) +
                         K.len() as u64) as usize));
        N.truncate(12);

        chacha = ChaCha20::new(&kPrime, &N[..], Some(self.parameters.r as i8));
        chacha.process(&mut input[..], &mut output[..]);

        // XXX Ugh… the .. syntax all over the place in this section is horribly unreadable.
        Key {
            S: take32(&output[..chachaLen]),
            N: toInts4(&output[chachaLen..][..nhLen].to_vec()).unwrap(),
            P: toInts8(&output[chachaLen + nhLen..][..polyLen].to_vec()).unwrap().iter()
                .map(|x| *x % 2u64.pow(60)).collect(),
            A: toInts8(&output[chachaLen + nhLen + polyLen..][..asuLen].to_vec()).unwrap(),
        }
    }
}

/// Hash `M` a total of `t` times with different keys and combine the result with the stream
/// cipher’s key.
///
/// # Inputs
///
/// - `k`, a vector `(KS, kN, kP, kA)`, where
///   - `kS`, is a string of 32 bytes,
///   - `kN`, is a vector of `b/4 + 4(t-1)` integers from ℤ_2^32,
///   - `kP`, is a vector of `t` integers from ℤ_2^60, and
///   - `kA`, is a vector of `3t` integers from ℤ_2^64,
/// - `M`, a string of any length,
/// - `N`, a 12-byte string,
/// - `y`, a integer in ℤ_2^38
///
/// # Output
///
/// - `Y`, a string of `y` bytes
///
/// # Algorithm
///
/// 1. A_i = HS1-Hash {b,t} (kN[4i, b/4], kP[i], kA[3i, 3], M) for each 0 ≤ i < t
/// 2. Y   = ChaCha {r} (pad(32, A_0 || A_1 || … || A_(t-1)) ⊕ kS), 0, N, 0^y)
///
/// # Side Channels
///
/// An adversary with the ability to conduct timing-based side channel attacks on a machine running
/// this code will some advantage to determine the Parameter set used (i.e. HS1_SIV_LO, HS1_SIV,
/// or HS1_SIV_HI).
///
/// # Example
/// ```
/// # #![allow(non_snake_case)]
/// # use crypto::hs1::{HS1, Subkeygen, PRF, HS1_SIV_HI, Key};
/// let hs1: HS1     = HS1::new(HS1_SIV_HI);
/// let k:   Key     = hs1.subkeygen(&([0x01; 32])[..]);
/// let M:   Vec<u8> = String::from("foo bar baz qux").into_bytes();
/// let N:   Vec<u8> = vec![0u8; 12];
/// let y:   i64     = 32;
///
/// assert_eq!(hs1.prf(&k, &M, &N, y),
///            vec![040, 090, 065, 216, 192, 028, 222, 229,
///                 130, 131, 243, 152, 171, 025, 152, 162,
///                 251, 238, 052, 245, 238, 146, 109, 036,
///                 222, 193, 077, 142, 232, 252, 236, 055])
/// ```
impl PRF for HS1 {
    fn prf(&self, k: &Key, M: &Vec<u8>, N: &Vec<u8>, y: i64) -> Vec<u8> {
        assert_eq!(N.len(), 12);
        assert!(0i64 < y);
        assert!(y < 2i64.pow(38));

        // "HS1 uses RFC 7539's chacha interface: https://tools.ietf.org/html/rfc7539
        // When the last parameter in a chacha call is n bytes, we have chacha produce n bytes and
        // then xor the last parameter with the chacha output. By having the last parameter be 0^y,
        // I am essentially having chacha produce y bytes." —Ted Krovetz
        //
        // So here, rather than xoring with a string of y 0-bytes, we'll just pad the key with
        // 0-bytes to a length of y to produce the input.
        let mut A:   Vec<u8> = Vec::with_capacity(self.parameters.t as usize);
        let mut key: Vec<u8> = repeat(0).take(32 as usize).collect();
        let mut Y:   Vec<u8> = repeat(0).take(y  as usize).collect();
        let input:   Vec<u8> = repeat(0).take(y  as usize).collect();

        // 1. `A_i = HS1-Hash[b,t](kN[4i, b/4], kP[i], kA[3i, 3], M) for each 0 ≤ i < t`
        for i in 0 .. self.parameters.t {
            let n: Vec<u32> = k.N[i as usize * 4 .. (i as usize * 4) + (self.parameters.b as usize / 4)].to_vec();
            let p: u64      = k.P[i as usize];
            let a: Vec<u64> = k.A[i as usize * 3 .. (i as usize * 3 + 3)].to_vec();

            // Concatenate A_i (either 4 or 8 bytes) into the hashed input for combination with the
            // keystream:
            let hashed = self.hash(&n, &p, &a, &M.clone());
            for byte in hashed.iter() {
                A.push(*byte);
            }
        }
        // 2. `Y   = ChaCha[r](pad(32, A_0 || A_1 || … || A_(t-1)) ⊕ kS), 0, N, 0^y)`
        xor_keystream(&mut key, &pad(32, &A), &k.S[..]);
        ChaCha20::new(&key[..], &N[..], Some(self.parameters.r as i8)).process(&input[..], &mut Y[..]);
        Y.to_vec()
    }
}

/// The hash family HS1-Hash is `(1/2^(28) + l/b2^(60))-AU` for all `M` up to `l` bytes, when
/// `k_N` and `k_P` are chosen randomly and `t ≤ 4`.
///
/// The hash family is `(1/2^(28) + 1/2^(32) + l/b2^(60))-SU` when additionally `k_A` is
/// randomly chosen and `t > 4`.  The extra 1/2^32 coming from Line 7, a strongly universal hash
/// developed by Lemire.
///
/// # Inputs
///
/// - `kN`, is a vector of `b/4` integers from ℤ_2^32,
/// - `kP`, is an integer from ℤ_2^60,
/// - `kA`, is a vector of 3 integers from ℤ_2^64,
/// - `M`, a string of any length.
///
/// # Output
///
/// - `Y`, an 8-byte (if t ≤ 4) or 4-byte (if t > 4) string.
///
/// # Adversarial Advantages
///
/// An adversary who can observe via some side channel, e.g. via differential power analysis or
/// timings, the execution of this function, can with some high probability determine the length of
/// the message, `M`.  This is due to a branching in step #3 of the underlying algorithm on
/// fixed-size chunks of the message.
///
/// # Algorithm:
///
/// 1. n = max(⌈|M|/b⌉, 1)
/// 2. M_1 || M_2 || … || M_n = M and |M_i| = b for each 1 ≤ i ≤ n.
/// 3. m_i = toInts(4, pad(16, M_i)) for each 1 ≤ i ≤ n.
/// 4. a_i = NH(kN, m_i) mod 2^60 + (|M_i| mod 16) for each 1 ≤ i ≤ n.
/// 5. h = kP^n + (a_1 × kP^(n-1)) + (a_2 × kP^(n-2)) + … + (a_n × kP^0) mod (2^61 - 1)
/// 6. if (t ≤ 4) Y = toStr(8, h)
/// 7. else Y = toStr(4, (kA[0] + kA[1] × (h mod 2^32) + kA[2] × (h div 2 ^32)) div 2^32)
impl Hash for HS1 {
    fn hash(&self, kN: &Vec<u32>, kP: &u64, kA: &Vec<u64>, M: &Vec<u8>) -> Vec<u8> {
        let n:     u32;
        let Mi:    Chunks<u8>;
        let mut Y: Vec<u8>;
        let mut a: Vec<BigInt> = Vec::new();
        let mut h: BigInt;
        let mut m: BigInt; // m is set to one of two moduli, each reused rather than recomputed.

        // 1. n = max(⌈|M|/b⌉, 1)
        n = std::cmp::max(M.len() as u32 / self.parameters.b as u32, 1);

        // 2. M_1 || M_2 || … || M_n = M and |M_i| = b for each 1 ≤ i ≤ n.
        Mi = M.chunks(self.parameters.b as usize);

        // 3. m_i = toInts(4, pad(16, M_i)) for each 1 ≤ i ≤ n.
        for (_, chunk) in Mi.enumerate() {
            let mi: Vec<u32> = toInts4(&pad(16, &chunk.to_vec())).unwrap();
            // 4. a_i = NH(kN, m_i) mod 2^60 + (|M_i| mod 16) for each 1 ≤ i ≤ n.
            a.push(NH(kN, &mi) + BigInt::from_u8(self.parameters.b % 16u8).unwrap());
        }
        // 5. h = kP^n + (a_1 × kP^(n-1)) + (a_2 × kP^(n-2)) + ... + (a_n × kP^0) mod (2^61 - 1)
        h = u64toBI!((*kP as u64).pow(n) % 2u64.pow(61)-1);
        m = u64toBI!(2u64.pow(61)-1);
        for (ai, j) in a.iter().zip(n as i32 .. 0) {
            h = h + (ai % m.clone()) * (u64toBI!(kP.pow(j as u32)) % m.clone()) % m.clone();
        }
        // 6. if (t ≤ 4) Y = toStr(8, h)
        if self.parameters.t <= 4 {
            Y = toStr(8, &(h.to_u64().unwrap() as usize));
            Y.truncate(8);
        } else {
        // 7. else Y = toStr(4, (kA[0] + kA[1] × (h mod 2^32) + kA[2] × (h div 2 ^32)) div 2^32)
            m = u64toBI!(2u64.pow(32));
            Y = toStr(4, &(((u64toBI!(kA[0].clone()) +
                             u64toBI!(kA[1].clone()) * (h.clone() % m.clone()) +
                             u64toBI!(kA[2].clone()) * (h.clone() / m.clone())) / m.clone())
                           .to_u64().unwrap() as usize));
            Y.truncate(4);
        }
        Y
    }
}

/// The `l`-byte string `T` serves as authenticator for `A` and `M`, and serves as IV for the
/// encryption of `M`.  If `N` is a nonce, then repeat encryptions yield different `T` and `C`.
/// Algorithm parameters `b`, `t`, `r`, and `l` effect security and performance.
///
/// # Inputs
///
/// - `K`, a non-empty string of up to 32 bytes,
/// - `M`, a string shorter than 2^64 bytes,
/// - `A`, a string shorter than 2^64 bytes,
/// - `N`, a 12-byte string
///
/// # Output
///
/// - `(T, C)`, strings of `l` and `|M|` bytes, respectively.
///
/// # Algorithm:
///
/// 1. k = HS1-subkeygen {b,t,r,l} (K)
/// 2. M' = pad(16, A) || pad(16, M) || toStr(8, |A|) || toStr(8, |M|)
/// 3. T = HS1 {b,t,r} (k, M', N, l)
/// 4. C = M ⊕ HS1 {b,t,r} (k, T, N, 64 + |M|)[64, |M|]
impl Encrypt for HS1 {
    fn encrypt(&self, K: &[u8], M: &Plaintext, A: &AssociatedData, N: &Vec<u8>)
               -> (Ciphertext, Authenticator) {
        assert!(N.len() == 12);

        let k:     Key;
        let m:     Vec<u8>;
        let mut T: Vec<u8>;
        let C:     Vec<u8>;

        k = self.subkeygen(&take32(K));  // OPTIMISE: maybe don't need take32()
        m = [pad(16, &A),
             pad(16, &M),
             toStr(8, &A.len()),
             toStr(8, &M.len())].concat();

        T = self.prf(&k, &m, &N, self.parameters.l as i64);
        T.truncate(self.parameters.l as usize);

        let intermediate: &[u8] = &self.prf(&k, &T, &N, (64 + M.len()) as i64);
        let mut output: Vec<u8> = repeat(0).take(intermediate.len()).collect();
        xor_keystream(&mut output[..], &pad(intermediate.len(), &M)[..], intermediate);
        C = output[64 .. 64 + M.len()].to_vec();

        assert_eq!(T.len(), self.parameters.l as usize);
        assert_eq!(C.len(), M.len());

        (T, C)
    }
}

// XXX_QUESTION: In the "Algorithm" section of Fig.5: Decryption, the numbering got all wonky.

/// The `l`-byte string `T` serves as authenticator for `A` and `M`, and serves as IV for the
/// decryption of `C`. Algorithm parameters `b`, `t`, `r`, and `l` effect security and
/// performance.
///
/// # Inputs:
///
/// - `K`, a non-empty string of up to 32 bytes,
/// - `(T, C)`, an `l`-byte string and a string shorter than 2^64 bytes, respectively.
/// - `A`, a string shorter than 2^64 bytes,
/// - `N`, a 12-byte string
///
/// # Output
///
/// - `M`, a `|C|`-byte string, or `AuthenticationError`.
///
/// # Algorithm:
///
/// 1. k = HS1-subkeygen {b,t,r,l} (K)
/// 2. M = C ⊕ HS1 {b,t,r} (k, T, N, 64 + |C|)[64, |C|]
/// 3. M' = pad(16, A) || pad(16, M) || toStr(8, |A|) || toStr(8, |M|)
/// 4. T' = HS1 {b,t,r} (k, M', N, l)
/// 5. if (T = T') then return M
/// 6. else return AuthenticationError
impl Decrypt for HS1 {
    fn decrypt(&self, K: &[u8], T: &Authenticator, C: &Ciphertext, A: &AssociatedData, N: &Vec<u8>)
               -> Result<Plaintext, Error> {
        assert!(T.len() == self.parameters.l as usize);
        assert!(N.len() == 12);

        let k:       Key;
        let M:       Vec<u8>;
        let m:       Vec<u8>;
        let t:       Vec<u8>;
        let mut out: Vec<u8> = repeat(0).take(C.len()).collect();

        k = self.subkeygen(&take32(K));

        // XXX_QUESTION: Here we are supposed to use `64 + C.len()` as the final parameter to prf(),
        // and then truncate the output to bytes between `[64 .. C.len()]`. However, because the `y`
        // parameter to prf() must equal 32 — as noted in a XXX_QUESTION above in prf() — we can't
        // do this.
        xor_keystream(&mut out[..], &C,
                      &self.prf(&k, &T, N, (64 + C.len()) as i64)[64 .. C.len()]);
        M = out.to_vec();
        m = [pad(16, &A),
             pad(16, &M),
             toStr(8, &A.len()),
             toStr(8, &M.len())].concat();
        t = self.prf(&k, &m, N, self.parameters.l as i64);

        if *T == t { Ok(M) } else { Err(Error::AuthenticationError) }
    }
}

// XXX_QUESTION: We moved the `mod 2^60` from hash() to NH() for simplicity… should this be changed
// in the spec?
//
// XXX_QUESTION: This is the only place in HS1-SIV which requires a bignum… everything else can get
// away with utilising either u32 or u64.  Should/can this be restructured to avoid needing a bignum?
//
/// Given vectors of integers, `v1` and `v2`, returns the result of the following algorithm:
///
/// ```text
///                   n/4 ⎛                                           ⎞
///     NH(v1, v2) =   Σ  ⎜(v1[4i-3]+v2[4i-3]) × (v1[4i-1]+v2[4i-1]) +⎟
///                   i=1 ⎝(v1[4i-2]+v2[4i-2]) × (v1[4i]+v2[4i])      ⎠
/// ```
/// where `n = min(v1.len(), v2.len())` and is alway a multiple of 4.
///
/// # Examples
/// ```
/// use crypto::hs1::{NH, BigInt, ToPrimitive};
///
/// let v1: Vec<u32> = vec![3189664965, 2714692993, 2994129442, 1858380706,
///                         1587789763, 415824430,  835318381,  2279956929,
///                         3870281273, 116792861,  285581825,  4002263835,
///                         2553110182, 676095448,  2497697706, 3646967354];
/// let v2: Vec<u32> = vec![543516756,  2003792483, 1768711712, 1629516645,
///                        1768759412,  1734962788, 3044456,    0];
///
/// assert_eq!(NH(&v1, &v2).to_u64().unwrap(), 162501409595406698u64);
/// ```
pub fn NH(v1: &Vec<u32>, v2: &Vec<u32>) -> BigInt {
    let mut sum: BigInt = BigInt::from_usize(0).unwrap();
    let m:       BigInt = BigInt::from_u64(2u64.pow(60)).unwrap();
    let bn1:     Vec<BigInt> = v1.iter().map(|x| x.to_bigint().unwrap()).collect();
    let bn2:     Vec<BigInt> = v2.iter().map(|x| x.to_bigint().unwrap()).collect();

    for i in 1 .. std::cmp::min(bn1.len(), bn2.len())/4 {
        sum = sum + (((&bn1[4 * i-3] + &bn2[4 * i-3]) * (&bn1[4 * i-1] + &bn2[4 * i-1]) +
                      (&bn1[4 * i-2] + &bn2[4 * i-2]) * (&bn1[4 * i  ] + &bn2[4 * i  ])) % &m);
    }
    sum
}

//-------------------------------------------------------------------------------------------------
//                                      Utility Functions
//-------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum ConversionError {
    /// Raised if there was some problem calling either `toInts4()` or `toInts8()` on a `String`.
    StrToInt,
    /// Raised if there was some problem calling either `toStr()` on an integer.
    IntToStr,
}

/// Pad the `input` to the given `length` by adding 0s to the end of the `input`.
///
/// # Inputs
///
///  * `length` is an integer specifying the length to pad to.
///  * `input` is a `vec::Vec<u8>` (a.k.a. a vector of octets).
///
/// # Examples
// TODO:  WTF, Rust?  Is there really no way to `use` a non-pub function within a doctest?
/// ```text
/// use crypto::hs1::pad;
///
/// let mut data: Vec<u8> = vec!([0x41, 0x42, 0x43]);
/// let padded = pad(5, &data);
///
/// assert_eq!(padded, [0x41, 0x42, 0x43, 0x00, 0x00]);
/// ```
fn pad(multiple: usize, input: &Vec<u8>) -> Vec<u8> {
    let mut padded: Vec<u8> = input.clone();

    while (padded.len() % multiple) > 0 {
        padded.push(0x00);
    }
    padded
}

/// `toStr(n, x)` is the `n`-byte unsigned little-endian binary representation of integer `x`.
///
/// # Examples
/// ```
/// use crypto::hs1::toStr;
///
/// let s1: Vec<u8> = toStr(4, &3);
/// assert!(vec![0, 0, 0, 3] == s1);
///
/// let s2: Vec<u8> = toStr(4, &256);
/// assert!(vec![0, 0, 1, 0] == s2);
///
/// let s3: Vec<u8> = toStr(4, &4294967295);
/// assert!(vec![255, 255, 255, 255] == s3);
/// ```
pub fn toStr<'a>(n: isize, x: &'a usize) -> Vec<u8> {
    let binary:       String  = format!("{:b}", x.to_le());
    let len:          isize = n * 8isize - binary.len() as isize;
    let mut bits:    BitVec = BitVec::from_fn(binary.len(), |i| { binary.char_at(i) == '1' });
    let mut padding: BitVec;

    if len > 0 {
        padding = BitVec::from_elem(len as usize, false);
        padding.extend((&mut bits).iter());
        return padding.to_bytes();
    } else {
        return bits.to_bytes();
    }
}

/// Returns a vector of integers obtained by breaking `S` into 4-byte chunks and little-endian
/// interpreting each chunk as as an unsigned integer.
///
/// # Examples
/// ```
/// use crypto::hs1::toInts4;
///
/// let result: Vec<u32> = toInts4(&vec![0, 0, 0, 3]).unwrap();
/// assert!(result[0] == 3u32);
///
/// let another: Vec<u32> = toInts4(&vec![0, 0, 0, 5, 0, 0, 0, 6]).unwrap();
/// assert!(another[0] == 5u32);
/// assert!(another[1] == 6u32);
///
/// let and_another: Vec<u32> = toInts4(&vec![0, 0, 1, 0]).unwrap();
/// assert!(and_another[0] == 256u32);
///
/// let yet_another: Vec<u32> = toInts4(&vec![255, 255, 255, 255]).unwrap();
/// assert!(yet_another[0] == 4294967295u32);
/// ```
pub fn toInts4(S: &Vec<u8>) -> Result<Vec<u32>, ConversionError> {
    if S.len() % 4 !=  0 {
        println!("The length of S in bytes must be some multiple of 4.");
        return Err(ConversionError::StrToInt);
    }
    let mut ints: Vec<u32> = Vec::new();

    unsafe {
        let chunks: std::slice::Chunks<u8> = S.chunks(4);
        for c in chunks {
            ints.push(std::mem::transmute::<[u8; 4], u32>([c[0], c[1], c[2], c[3]]).to_be());
        }
    }
    Ok(ints)
}

/// Returns a vector of integers obtained by breaking `S` into 8-byte chunks and little-endian
/// interpreting each chunk as as an unsigned integer.
///
/// # Examples
/// ```
/// use crypto::hs1::toInts8;
///
/// let result: Vec<u64> = toInts8(&vec![0, 0, 0, 0, 0, 0, 0, 3]).unwrap();
/// assert!(result[0] == 3u64);
///
/// let another: Vec<u64> = toInts8(&vec![0, 0, 0, 5, 0, 0, 0, 6, 255, 255, 255, 255, 0, 0, 1, 0]).unwrap();
/// println!("another = {:?}", another);
/// assert!(another[0] == 21474836486u64);
/// assert!(another[1] == 18446744069414584576u64);
///
/// let and_another: Vec<u64> = toInts8(&vec![0, 0, 0, 0, 0, 0, 1, 0]).unwrap();
/// assert!(and_another[0] == 256u64);
///
/// let yet_another: Vec<u64> = toInts8(&vec![255, 255, 255, 255, 255, 255, 255, 255]).unwrap();
/// assert!(yet_another[0] == 18446744073709551615u64);
/// ```
pub fn toInts8(S: &Vec<u8>) -> Result<Vec<u64>, ConversionError> {
    if S.len() % 8 !=  0 {
        println!("The length of S in bytes must be some multiple of 8.");
        return Err(ConversionError::StrToInt);
    }
    let mut ints: Vec<u64> = Vec::new();

    unsafe {
        let chunks: std::slice::Chunks<u8> = S.chunks(8);
        for c in chunks {
            ints.push(std::mem::transmute::<[u8; 8], u64>([c[0], c[1], c[2], c[3],
                                                           c[4], c[5], c[6], c[7]]).to_be());
        }
    }
    Ok(ints)
}

#[inline]
fn take32 <'a> (x: &'a [u8]) -> [u8; 32] {
    assert!(x.len() >= 12);
    [x[0],  x[1],  x[2],  x[3],
     x[4],  x[5],  x[6],  x[7],
     x[8],  x[9],  x[10], x[11],
     x[12], x[13], x[14], x[15],
     x[16], x[17], x[18], x[19],
     x[20], x[21], x[22], x[23],
     x[24], x[25], x[26], x[27],
     x[28], x[29], x[30], x[31]]
}

//-------------------------------------------------------------------------------------------------
//                                          Tests
//-------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use hs1::*;
    use std::iter::repeat;

    static KEY_32_BYTES: [u8; 32] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        0x09, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
        0x17, 0x18, 0x19, 0x20, 0x21, 0x22, 0x23, 0x24,
        0x25, 0x26, 0x27, 0x28, 0x29, 0x30, 0x31, 0x32,];

    fn nonce() -> Vec<u8> { repeat(0).take(12).collect() }
    fn msg() -> Vec<u8> { String::from("The crow flies at midnight.").into_bytes() }
    fn associated_data() -> Vec<u8> { String::from("Nomnomnom data").into_bytes() }

    #[test]
    fn test_hs1_toStr_toInts4_3() {
        let orig: usize = 3;
        assert_eq!(toInts4(&toStr(4, &orig)).unwrap()[0], orig as u32);
    }
    #[test]
    fn test_hs1_toStr_toInts4_256() {
        let orig: usize = 256;
        assert_eq!(toInts4(&toStr(4, &orig)).unwrap()[0], orig as u32);
    }
    #[test]
    fn test_hs1_toStr_toInts4_4294967295() {
        let orig: usize = 4294967295;
        assert_eq!(toInts4(&toStr(4, &orig)).unwrap()[0], orig as u32);
    }
    #[test]
    fn test_hs1_toStr_toInts8_3() {
        let orig: usize = 3;
        assert_eq!(toInts8(&toStr(8, &orig)).unwrap()[0], orig as u64);
    }
    #[test]
    fn test_hs1_toStr_toInts8_256() {
        let orig: usize = 256;
        assert_eq!(toInts8(&toStr(8, &orig)).unwrap()[0], orig as u64);
    }
    #[test]
    fn test_hs1_toStr_toInts8_18446744073709551615() {
        let orig: usize = 18446744073709551615;
        assert_eq!(toInts8(&toStr(8, &orig)).unwrap()[0], orig as u64);
    }

    #[test]
    fn test_hs1_siv_lo_init() { HS1::new(HS1_SIV_LO); }
    #[test]
    fn test_hs1_siv_init()    { HS1::new(HS1_SIV);    }
    #[test]
    fn test_hs1_siv_hi_init() { HS1::new(HS1_SIV_HI); }

    #[test]
    fn test_hs1_siv_lo_subkeygen() {
        let hs1: HS1 = HS1::new(HS1_SIV_LO);
        let key: Key = hs1.subkeygen(&KEY_32_BYTES[..]);

        let expected_key: Key = Key {
            S: [029, 228, 104, 037, 180, 170, 202, 148,
                039, 029, 011, 119, 052, 228, 162, 079,
                128, 010, 066, 099, 030, 015, 033, 081,
                069, 168, 127, 167, 237, 243, 229, 077],
            N: vec![2660748217, 2558697155, 1714681981, 3657416328,
                    3435480299, 3645588291, 2921336019, 3151644245,
                    1846274549, 733311385,  3992634613, 1373378016,
                    2044750242, 3861933274, 3504758271, 2948714715,
                    988945780,  2774551967, 1716400572, 2887958241],
            P: vec![645997590300049322, 322522510819694843],
            A: vec![14529885378449895274, 12653303077038689006, 4892742607418050275,
                    5567500722073670002,  6892266127813746098, 3448115731693163942],
        };
        assert_eq!(expected_key, key);
    }

    #[test]
    fn test_hs1_siv_hi_subkeygen() {
        let hs1: HS1 = HS1::new(HS1_SIV_HI);
        let key: Key = hs1.subkeygen(&KEY_32_BYTES[..]);

        let expected_key: Key = Key {
            S: [183, 203, 207, 198, 065, 127, 180, 010,
                062, 177, 006, 231, 182, 034, 248, 155,
                128, 204, 222, 205, 073, 134, 222, 225,
                212, 089, 026, 111, 215, 000, 189, 158],
            N: vec![0538054445, 3815201821, 1658273067, 2006661402,
                    1502798711, 3216001020, 2791718294, 4158839867,
                    1319188268, 2638818572, 3644886270, 4019180444,
                    3870353244, 3868178272, 0788204393, 4190300094,
                    0945830914, 0464429597, 1545514780, 3965556729,
                    1313744640, 1477009656, 1226266952, 4227077356,
                    0901828758, 1197816774, 1127449543, 2512935124,
                    2771301304, 2077689234, 0535212884, 3260924812,
                    2790804429, 4073305765, 0681150126, 2667672114],
            P: vec![676683954131899665, 895992662275865463, 811712090461866245,
                    441402635191776148, 975454427191395514, 566086767741485818],
            A: vec![13166067761422627960, 05153395817732499259, 1274447141331486434,
                    10110585516739992835, 16073407688598778655, 7542607024747653607,
                    07638003855746033839, 07762141420122725837, 16832064194217617262,
                    16812346987467787277, 02465464082020945580, 7918974157707067963,
                    14820733191035548111, 13229803496537705501, 16383354179382607506,
                    13241778947485406112, 04826904851142858747, 9817695108583611981],
        };
        assert_eq!(expected_key, key);
    }

    #[test]
    fn test_hs1_siv_lo_prf() {
        let hs1: HS1     = HS1::new(HS1_SIV_LO);
        let key: Key     = hs1.subkeygen(&KEY_32_BYTES[..]);
        let prf: Vec<u8> = hs1.prf(&key, &msg(), &nonce(), 32i64);
        assert_eq!(&prf[..],
                   [047, 018, 134, 029, 115, 204, 003, 173,
                    153, 037, 060, 052, 019, 085, 232, 126,
                    198, 134, 077, 036, 161, 069, 154, 062,
                    145, 144, 097, 236, 171, 003, 189, 145])
    }

    #[test]
    fn test_hs1_siv_prf() {
        let hs1: HS1     = HS1::new(HS1_SIV);
        let key: Key     = hs1.subkeygen(&KEY_32_BYTES[..]);
        let prf: Vec<u8> = hs1.prf(&key, &msg(), &nonce(), 32i64);
        assert_eq!(&prf[..],
                   [101, 236, 097, 169, 244, 020, 011, 142,
                    225, 180, 192, 190, 069, 018, 089, 229,
                    071, 008, 179, 156, 122, 091, 115, 236,
                    213, 101, 180, 231, 253, 200, 083, 062])
    }

    #[test]
    fn test_hs1_siv_hi_prf() {
        let hs1: HS1     = HS1::new(HS1_SIV_HI);
        let key: Key     = hs1.subkeygen(&KEY_32_BYTES[..]);

        // That's what the acronym is short for, right?
        let punk_rock_forever: Vec<u8> = hs1.prf(&key, &msg(), &nonce(), 32i64);

        assert_eq!(&punk_rock_forever[..],
                   [036, 015, 088, 013, 151, 193, 179, 129,
                    118, 223, 064, 211, 223, 156, 223, 218,
                    166, 200, 152, 151, 212, 214, 239, 251,
                    120, 107, 008, 159, 076, 118, 174, 17])
    }

    #[test]
    fn test_hs1_siv_lo_hash() {
        let hs1:  HS1     = HS1::new(HS1_SIV_LO);
        let key:  Key     = hs1.subkeygen(&KEY_32_BYTES[..]);
        let hash: Vec<u8> = hs1.hash(&key.N, &32u64, &key.A, &msg());
        assert_eq!(&hash[..], [0, 0, 0, 0, 0, 0, 0, 31])
    }

    #[test]
    fn test_hs1_siv_hash() {
        let hs1:  HS1     = HS1::new(HS1_SIV);
        let key:  Key     = hs1.subkeygen(&KEY_32_BYTES[..]);
        let hash: Vec<u8> = hs1.hash(&key.N, &32u64, &key.A, &msg());
        assert_eq!(&hash[..], [0, 0, 0, 0, 0, 0, 0, 31])
    }

    #[test]
    fn test_hs1_siv_hi_hash() {
        let hs1:  HS1     = HS1::new(HS1_SIV_HI);
        let key:  Key     = hs1.subkeygen(&KEY_32_BYTES[..]);
        let hash: Vec<u8> = hs1.hash(&key.N, &32u64, &key.A, &msg());
        assert_eq!(&hash[..], [149, 252, 064, 013])
    }

    #[test]
    fn test_hs1_siv_lo_encrypt() {
        let hs1: HS1 = HS1::new(HS1_SIV_LO);
        let ca: (Vec<u8>, Vec<u8>) = hs1.encrypt(&KEY_32_BYTES[..], &msg(), &associated_data(), &nonce());

        assert_eq!(ca.0, vec![047, 018, 134, 029, 115, 204, 003, 173]); // authentication data
        assert_eq!(ca.1, vec![052, 224, 254, 195, 022, 240, 040, 202, 064,
                              099, 103, 229, 032, 098, 174, 058, 242, 235,
                              048, 081, 171, 165, 120, 133, 035, 113, 073]); // encrypted data
    }

    #[test]
    fn test_hs1_siv_encrypt() {
        let hs1: HS1 = HS1::new(HS1_SIV);
        let ca: (Vec<u8>, Vec<u8>) = hs1.encrypt(&KEY_32_BYTES[..], &msg(), &associated_data(), &nonce());

        assert_eq!(ca.0, vec![101, 236, 097, 169, 244, 020, 011, 142,
                              225, 180, 192, 190, 069, 018, 089, 229]);
        assert_eq!(ca.1, vec![160, 214, 124, 208, 100, 032, 204, 068, 225,
                              066, 174, 027, 129, 105, 253, 027, 124, 110,
                              042, 174, 102, 111, 124, 198, 191, 192, 238]);
    }

    #[test]
    fn test_hs1_siv_hi_encrypt() {
        let hs1: HS1 = HS1::new(HS1_SIV_HI);
        let ca: (Vec<u8>, Vec<u8>) = hs1.encrypt(&KEY_32_BYTES[..], &msg(), &associated_data(), &nonce());

        assert_eq!(ca.0, vec![036, 015, 088, 013, 151, 193, 179, 129,
                              118, 223, 064, 211, 223, 156, 223, 218,
                              166, 200, 152, 151, 212, 214, 239, 251,
                              120, 107, 008, 159, 076, 118, 174, 017]);
        assert_eq!(ca.1, vec![171, 153, 220, 057, 213, 086, 048, 015, 153,
                              197, 154, 124, 153, 072, 212, 170, 008, 222,
                              217, 221, 193, 000, 226, 172, 109, 242, 195]);
    }

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn test_hs1_siv_lo_decrypt() {
        let hs1: HS1 = HS1::new(HS1_SIV_LO);

        // XXX these are probably wrong because of the problems within the PRF() function's specification
        let a: Vec<u8> = vec![49, 126, 200, 176, 47, 149, 122, 127];
        let e: Vec<u8> = vec![101, 22, 173, 144, 76, 231, 21, 8, 125,
                              201, 146, 17, 137, 73, 143, 212, 145, 250,
                              38, 3, 89, 176, 163, 47, 89, 88, 159];
        let d: Result<Plaintext, Error> = hs1.decrypt(&KEY_32_BYTES[..], &a, &e,
                                                      &associated_data(), &nonce());
        assert_eq!(&d.unwrap()[..], &msg()[..]);
    }
}
