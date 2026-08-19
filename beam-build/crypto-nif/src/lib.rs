//! Tyn's replacement `:crypto` NIF (Option A). Implements exactly the surface
//! Phoenix/Plug use (Step 3), with vetted RustCrypto crates. no_std; allocation
//! is routed to enif_alloc/enif_free so RustCrypto's Vec-returning APIs work.
//!
//! Archive `crypto.a` -> init `crypto_nif_init`, module "crypto"; the shim
//! crypto.erl (replacing OTP's) load_nif("crypto") resolves it from the static
//! table. randomness comes from getrandom(2) -> the kernel CSPRNG (src/rng.rs).

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use subtle::ConstantTimeEq;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20poly1305::ChaCha20Poly1305;

// ---------- global allocator over enif_alloc/enif_free ----------
struct EnifAlloc;
const WORD: usize = core::mem::size_of::<usize>();

unsafe impl GlobalAlloc for EnifAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(WORD);
        let total = layout.size() + align + WORD;
        let raw = enif_alloc(total) as usize;
        if raw == 0 {
            return core::ptr::null_mut();
        }
        // aligned block, with room to stash `raw` in the word just below it.
        let aligned = (raw + WORD + align - 1) & !(align - 1);
        *((aligned - WORD) as *mut usize) = raw;
        aligned as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let raw = *((ptr as usize - WORD) as *mut usize);
        enif_free(raw as *mut c_void);
    }
}
#[global_allocator]
static ALLOC: EnifAlloc = EnifAlloc;

// ---------- erl_nif ABI (NIF 2.17, OTP 27) ----------
type ErlNifTerm = usize;
#[repr(C)]
pub struct ErlNifEnv {
    _p: [u8; 0],
}
#[repr(C)]
struct ErlNifBinary {
    size: usize,
    data: *mut u8,
    ref_bin: *mut c_void,
    spare: [*mut c_void; 2],
}
#[repr(C)]
struct ErlNifFunc {
    name: *const c_char,
    arity: c_uint,
    fptr: extern "C" fn(*mut ErlNifEnv, c_int, *const ErlNifTerm) -> ErlNifTerm,
    flags: c_uint,
}
type LoadFn = extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, ErlNifTerm) -> c_int;
type UnloadFn = extern "C" fn(*mut ErlNifEnv, *mut c_void);
type UpgradeFn = extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, *mut *mut c_void, ErlNifTerm) -> c_int;

#[repr(C)]
struct ErlNifEntry {
    major: c_int,
    minor: c_int,
    name: *const c_char,
    num_of_funcs: c_int,
    funcs: *const ErlNifFunc,
    load: Option<LoadFn>,
    reload: Option<LoadFn>,
    upgrade: Option<UpgradeFn>,
    unload: Option<UnloadFn>,
    vm_variant: *const c_char,
    options: c_uint,
    sizeof_resource_type_init: usize,
    min_erts: *const c_char,
}

const ERL_NIF_LATIN1: c_int = 1;
const DIRTY_CPU: c_uint = 1; // ERL_NIF_DIRTY_JOB_CPU_BOUND

extern "C" {
    fn enif_alloc(size: usize) -> *mut c_void;
    fn enif_free(ptr: *mut c_void);
    fn enif_inspect_binary(env: *mut ErlNifEnv, term: ErlNifTerm, bin: *mut ErlNifBinary) -> c_int;
    // Accepts a binary OR an iolist (Phoenix/Plug pass iodata to hash/mac).
    fn enif_inspect_iolist_as_binary(env: *mut ErlNifEnv, term: ErlNifTerm, bin: *mut ErlNifBinary) -> c_int;
    fn enif_make_new_binary(env: *mut ErlNifEnv, size: usize, termp: *mut ErlNifTerm) -> *mut u8;
    fn enif_get_atom(env: *mut ErlNifEnv, term: ErlNifTerm, buf: *mut c_char, len: c_uint, enc: c_int) -> c_int;
    fn enif_make_atom(env: *mut ErlNifEnv, name: *const c_char) -> ErlNifTerm;
    fn enif_get_ulong(env: *mut ErlNifEnv, term: ErlNifTerm, ip: *mut c_ulong) -> c_int;
    fn enif_make_badarg(env: *mut ErlNifEnv) -> ErlNifTerm;
    fn enif_make_tuple_from_array(env: *mut ErlNifEnv, arr: *const ErlNifTerm, cnt: c_uint) -> ErlNifTerm;
    fn getrandom(buf: *mut u8, len: usize, flags: c_uint) -> isize;
}

// ---------- marshalling helpers ----------
unsafe fn get_bin<'a>(env: *mut ErlNifEnv, term: ErlNifTerm) -> Option<&'a [u8]> {
    let mut b = ErlNifBinary {
        size: 0,
        data: core::ptr::null_mut(),
        ref_bin: core::ptr::null_mut(),
        spare: [core::ptr::null_mut(); 2],
    };
    // iolist_as_binary accepts both plain binaries and iolists (iodata).
    if enif_inspect_iolist_as_binary(env, term, &mut b) == 0 {
        return None;
    }
    Some(core::slice::from_raw_parts(b.data, b.size))
}

unsafe fn make_bin(env: *mut ErlNifEnv, data: &[u8]) -> ErlNifTerm {
    let mut term: ErlNifTerm = 0;
    let ptr = enif_make_new_binary(env, data.len(), &mut term);
    if !ptr.is_null() && !data.is_empty() {
        core::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    }
    term
}

unsafe fn get_atom<'a>(env: *mut ErlNifEnv, term: ErlNifTerm, buf: &'a mut [u8]) -> Option<&'a str> {
    let n = enif_get_atom(env, term, buf.as_mut_ptr() as *mut c_char, buf.len() as c_uint, ERL_NIF_LATIN1);
    if n <= 0 {
        return None;
    }
    core::str::from_utf8(&buf[..(n as usize - 1)]).ok()
}

unsafe fn get_uint(env: *mut ErlNifEnv, term: ErlNifTerm) -> Option<u64> {
    let mut v: c_ulong = 0;
    if enif_get_ulong(env, term, &mut v) == 0 {
        return None;
    }
    Some(v as u64)
}

unsafe fn atom(env: *mut ErlNifEnv, s: &str) -> ErlNifTerm {
    // s must be a static NUL-terminated-able ascii; we build a small stack buf.
    let mut buf = [0u8; 24];
    let n = s.len().min(23);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    enif_make_atom(env, buf.as_ptr() as *const c_char)
}

unsafe fn tuple2(env: *mut ErlNifEnv, a: ErlNifTerm, b: ErlNifTerm) -> ErlNifTerm {
    let arr = [a, b];
    enif_make_tuple_from_array(env, arr.as_ptr(), 2)
}

macro_rules! badarg {
    ($env:expr) => {
        return unsafe { enif_make_badarg($env) }
    };
}

// ---------- NIFs ----------

// strong_rand_bytes(N) -> binary
extern "C" fn nif_strong_rand_bytes(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 1 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 1);
        let n = match get_uint(env, a[0]) {
            Some(n) => n as usize,
            None => badarg!(env),
        };
        let mut term: ErlNifTerm = 0;
        let ptr = enif_make_new_binary(env, n, &mut term);
        let mut off = 0usize;
        while off < n {
            let r = getrandom(ptr.add(off), n - off, 0);
            if r <= 0 {
                badarg!(env);
            }
            off += r as usize;
        }
        term
    }
}

// hash(Type, Data) -> binary
extern "C" fn nif_hash(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 2);
        let mut ab = [0u8; 24];
        let ty = match get_atom(env, a[0], &mut ab) {
            Some(s) => s,
            None => badarg!(env),
        };
        let data = match get_bin(env, a[1]) {
            Some(d) => d,
            None => badarg!(env),
        };
        match ty {
            "sha" => make_bin(env, &Sha1::digest(data)),
            "sha224" => make_bin(env, &Sha224::digest(data)),
            "sha256" => make_bin(env, &Sha256::digest(data)),
            "sha384" => make_bin(env, &Sha384::digest(data)),
            "sha512" => make_bin(env, &Sha512::digest(data)),
            _ => enif_make_badarg(env),
        }
    }
}

// mac(hmac, SubType, Key, Data) -> binary
extern "C" fn nif_mac(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 4 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 4);
        let mut b0 = [0u8; 24];
        let mut b1 = [0u8; 24];
        let kind = match get_atom(env, a[0], &mut b0) {
            Some(s) => s,
            None => badarg!(env),
        };
        if kind != "hmac" {
            badarg!(env);
        }
        let sub = match get_atom(env, a[1], &mut b1) {
            Some(s) => s,
            None => badarg!(env),
        };
        let key = match get_bin(env, a[2]) {
            Some(k) => k,
            None => badarg!(env),
        };
        let data = match get_bin(env, a[3]) {
            Some(d) => d,
            None => badarg!(env),
        };
        macro_rules! hmac_do {
            ($h:ty) => {{
                let mut m = <Hmac<$h> as Mac>::new_from_slice(key).unwrap();
                m.update(data);
                make_bin(env, &m.finalize().into_bytes())
            }};
        }
        match sub {
            "sha" => hmac_do!(Sha1),
            "sha224" => hmac_do!(Sha224),
            "sha256" => hmac_do!(Sha256),
            "sha384" => hmac_do!(Sha384),
            "sha512" => hmac_do!(Sha512),
            _ => enif_make_badarg(env),
        }
    }
}

// pbkdf2_hmac(Digest, Password, Salt, Iterations, Length) -> binary
extern "C" fn nif_pbkdf2_hmac(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 5 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 5);
        let mut b0 = [0u8; 24];
        let digest = match get_atom(env, a[0], &mut b0) {
            Some(s) => s,
            None => badarg!(env),
        };
        let pw = match get_bin(env, a[1]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let salt = match get_bin(env, a[2]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let iters = match get_uint(env, a[3]) {
            Some(x) => x as u32,
            None => badarg!(env),
        };
        let len = match get_uint(env, a[4]) {
            Some(x) => x as usize,
            None => badarg!(env),
        };
        let mut out = vec![0u8; len];
        match digest {
            "sha" => pbkdf2::pbkdf2_hmac::<Sha1>(pw, salt, iters, &mut out),
            "sha224" => pbkdf2::pbkdf2_hmac::<Sha224>(pw, salt, iters, &mut out),
            "sha256" => pbkdf2::pbkdf2_hmac::<Sha256>(pw, salt, iters, &mut out),
            "sha384" => pbkdf2::pbkdf2_hmac::<Sha384>(pw, salt, iters, &mut out),
            "sha512" => pbkdf2::pbkdf2_hmac::<Sha512>(pw, salt, iters, &mut out),
            _ => badarg!(env),
        }
        make_bin(env, &out)
    }
}

// exor(Bin1, Bin2) -> binary
extern "C" fn nif_exor(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 2);
        let b1 = match get_bin(env, a[0]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let b2 = match get_bin(env, a[1]) {
            Some(x) => x,
            None => badarg!(env),
        };
        if b1.len() != b2.len() {
            badarg!(env);
        }
        let mut out = vec![0u8; b1.len()];
        for i in 0..b1.len() {
            out[i] = b1[i] ^ b2[i];
        }
        make_bin(env, &out)
    }
}

// hash_equals(Bin1, Bin2) -> boolean (constant time)
extern "C" fn nif_hash_equals(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, 2);
        let b1 = match get_bin(env, a[0]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let b2 = match get_bin(env, a[1]) {
            Some(x) => x,
            None => badarg!(env),
        };
        // :crypto.hash_equals requires equal length; different lengths -> false.
        if b1.len() != b2.len() {
            return atom(env, "false");
        }
        let eq: bool = b1.ct_eq(b2).into();
        atom(env, if eq { "true" } else { "false" })
    }
}

// crypto_one_time_aead: /6 encrypt (Cipher,Key,IV,Plain,AAD,true) -> {Cipher,Tag}
//                       /7 decrypt (Cipher,Key,IV,Ct,AAD,Tag,false) -> Plain | error
extern "C" fn nif_aead(env: *mut ErlNifEnv, argc: c_int, argv: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 6 && argc != 7 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(argv, argc as usize);
        let mut cb = [0u8; 24];
        let cipher = match get_atom(env, a[0], &mut cb) {
            Some(s) => s,
            None => badarg!(env),
        };
        let key = match get_bin(env, a[1]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let iv = match get_bin(env, a[2]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let intext = match get_bin(env, a[3]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let aad = match get_bin(env, a[4]) {
            Some(x) => x,
            None => badarg!(env),
        };
        if iv.len() != 12 {
            badarg!(env); // all supported AEADs use a 12-byte nonce
        }
        let nonce = GenericArray::from_slice(iv);
        let encrypt = argc == 6;

        // returns Some((ct, tag)) for encrypt, Some(pt) for decrypt (with tag)
        macro_rules! run {
            ($ty:ty) => {{
                let c = match <$ty>::new_from_slice(key) {
                    Ok(c) => c,
                    Err(_) => badarg!(env),
                };
                if encrypt {
                    let mut buf = intext.to_vec();
                    match c.encrypt_in_place_detached(nonce, aad, &mut buf) {
                        Ok(tag) => tuple2(env, make_bin(env, &buf), make_bin(env, &tag)),
                        Err(_) => enif_make_badarg(env),
                    }
                } else {
                    let tag = match get_bin(env, a[5]) {
                        Some(t) => t,
                        None => badarg!(env),
                    };
                    let mut buf = intext.to_vec();
                    let tag_ga = GenericArray::from_slice(tag);
                    match c.decrypt_in_place_detached(nonce, aad, &mut buf, tag_ga) {
                        Ok(()) => make_bin(env, &buf),
                        Err(_) => atom(env, "error"), // tag mismatch -> fail closed
                    }
                }
            }};
        }
        match cipher {
            "aes_128_gcm" => run!(Aes128Gcm),
            "aes_256_gcm" => run!(Aes256Gcm),
            "chacha20_poly1305" => run!(ChaCha20Poly1305),
            _ => enif_make_badarg(env),
        }
    }
}

// ---------- A' outbound: ECDHE for OTP :ssl ----------
// crypto:generate_key(ecdh, Curve) -> {Public, Private} in OTP's encodings:
//   x25519    -> {raw 32-byte public, raw 32-byte private}
//   secp256r1 -> {uncompressed point <<4,X,Y>> (65B), scalar (32B)}
//   secp384r1 -> {uncompressed point (97B), scalar (48B)}
extern "C" fn nif_generate_key(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    use p256::elliptic_curve::sec1::ToSec1Point;
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(a, 2);
        let mut tb = [0u8; 24];
        let mut cb = [0u8; 24];
        if get_atom(env, a[0], &mut tb) != Some("ecdh") {
            badarg!(env);
        }
        let curve = match get_atom(env, a[1], &mut cb) {
            Some(c) => c,
            None => badarg!(env),
        };
        match curve {
            "x25519" => {
                let mut sk = [0u8; 32];
                if getrandom(sk.as_mut_ptr(), 32, 0) != 32 {
                    badarg!(env);
                }
                let secret = x25519_dalek::StaticSecret::from(sk);
                let public = x25519_dalek::PublicKey::from(&secret);
                tuple2(env, make_bin(env, public.as_bytes()), make_bin(env, &secret.to_bytes()))
            }
            "secp256r1" => loop {
                let mut fb = [0u8; 32];
                if getrandom(fb.as_mut_ptr(), 32, 0) != 32 {
                    badarg!(env);
                }
                if let Ok(sk) = p256::SecretKey::from_slice(&fb) {
                    let pt = sk.public_key().as_affine().to_sec1_point(false);
                    return tuple2(env, make_bin(env, pt.as_bytes()), make_bin(env, sk.to_bytes().as_slice()));
                }
            },
            "secp384r1" => loop {
                let mut fb = [0u8; 48];
                if getrandom(fb.as_mut_ptr(), 48, 0) != 48 {
                    badarg!(env);
                }
                if let Ok(sk) = p384::SecretKey::from_slice(&fb) {
                    let pt = sk.public_key().as_affine().to_sec1_point(false);
                    return tuple2(env, make_bin(env, pt.as_bytes()), make_bin(env, sk.to_bytes().as_slice()));
                }
            },
            _ => badarg!(env),
        }
    }
}

// crypto:compute_key(ecdh, PeerPublic, MyPrivate, Curve) -> SharedSecret
//   x25519 -> 32-byte shared; NIST -> x-coordinate of the shared point.
extern "C" fn nif_compute_key(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 4 {
            badarg!(env);
        }
        let a = core::slice::from_raw_parts(a, 4);
        let mut tb = [0u8; 24];
        let mut cb = [0u8; 24];
        if get_atom(env, a[0], &mut tb) != Some("ecdh") {
            badarg!(env);
        }
        let peer = match get_bin(env, a[1]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let mine = match get_bin(env, a[2]) {
            Some(x) => x,
            None => badarg!(env),
        };
        let curve = match get_atom(env, a[3], &mut cb) {
            Some(c) => c,
            None => badarg!(env),
        };
        match curve {
            "x25519" => {
                if mine.len() != 32 || peer.len() != 32 {
                    badarg!(env);
                }
                let mut s = [0u8; 32];
                s.copy_from_slice(mine);
                let mut p = [0u8; 32];
                p.copy_from_slice(peer);
                let secret = x25519_dalek::StaticSecret::from(s);
                let peerpub = x25519_dalek::PublicKey::from(p);
                let shared = secret.diffie_hellman(&peerpub);
                make_bin(env, shared.as_bytes())
            }
            "secp256r1" => {
                let sk = match p256::SecretKey::from_slice(mine) {
                    Ok(s) => s,
                    Err(_) => badarg!(env),
                };
                let pk = match p256::PublicKey::from_sec1_bytes(peer) {
                    Ok(p) => p,
                    Err(_) => badarg!(env),
                };
                let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
                make_bin(env, shared.raw_secret_bytes().as_slice())
            }
            "secp384r1" => {
                let sk = match p384::SecretKey::from_slice(mine) {
                    Ok(s) => s,
                    Err(_) => badarg!(env),
                };
                let pk = match p384::PublicKey::from_sec1_bytes(peer) {
                    Ok(p) => p,
                    Err(_) => badarg!(env),
                };
                let shared = p384::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
                make_bin(env, shared.raw_secret_bytes().as_slice())
            }
            _ => badarg!(env),
        }
    }
}

// ---------- entry ----------
const NAME: &[u8] = b"crypto\0";
const VM_VARIANT: &[u8] = b"beam.vanilla\0";
const MIN_ERTS: &[u8] = b"erts-14.0\0";
const F_RAND: &[u8] = b"strong_rand_bytes\0";
const F_HASH: &[u8] = b"hash\0";
const F_MAC: &[u8] = b"mac\0";
const F_PBKDF2: &[u8] = b"pbkdf2_hmac\0";
const F_EXOR: &[u8] = b"exor\0";
const F_HEQ: &[u8] = b"hash_equals\0";
const F_AEAD: &[u8] = b"crypto_one_time_aead\0";
const F_GENKEY: &[u8] = b"generate_key\0";
const F_COMPUTEKEY: &[u8] = b"compute_key\0";

struct Sync<T>(T);
unsafe impl<T> core::marker::Sync for Sync<T> {}

static FUNCS: Sync<[ErlNifFunc; 10]> = Sync([
    ErlNifFunc { name: F_RAND.as_ptr() as *const c_char, arity: 1, fptr: nif_strong_rand_bytes, flags: 0 },
    ErlNifFunc { name: F_HASH.as_ptr() as *const c_char, arity: 2, fptr: nif_hash, flags: 0 },
    ErlNifFunc { name: F_MAC.as_ptr() as *const c_char, arity: 4, fptr: nif_mac, flags: 0 },
    // PBKDF2 can be milliseconds at Phoenix's iteration count -> dirty CPU NIF
    // so it never blocks a normal scheduler thread.
    ErlNifFunc { name: F_PBKDF2.as_ptr() as *const c_char, arity: 5, fptr: nif_pbkdf2_hmac, flags: DIRTY_CPU },
    ErlNifFunc { name: F_EXOR.as_ptr() as *const c_char, arity: 2, fptr: nif_exor, flags: 0 },
    ErlNifFunc { name: F_HEQ.as_ptr() as *const c_char, arity: 2, fptr: nif_hash_equals, flags: 0 },
    ErlNifFunc { name: F_AEAD.as_ptr() as *const c_char, arity: 6, fptr: nif_aead, flags: 0 },
    ErlNifFunc { name: F_AEAD.as_ptr() as *const c_char, arity: 7, fptr: nif_aead, flags: 0 },
    // A' ECDHE (dirty CPU — asymmetric keygen/ECDH is heavier than symmetric ops).
    ErlNifFunc { name: F_GENKEY.as_ptr() as *const c_char, arity: 2, fptr: nif_generate_key, flags: DIRTY_CPU },
    ErlNifFunc { name: F_COMPUTEKEY.as_ptr() as *const c_char, arity: 4, fptr: nif_compute_key, flags: DIRTY_CPU },
]);

static ENTRY: Sync<ErlNifEntry> = Sync(ErlNifEntry {
    major: 2,
    minor: 17,
    name: NAME.as_ptr() as *const c_char,
    num_of_funcs: 10,
    funcs: FUNCS.0.as_ptr(),
    load: None,
    reload: None,
    upgrade: None,
    unload: None,
    vm_variant: VM_VARIANT.as_ptr() as *const c_char,
    options: 1,
    sizeof_resource_type_init: 40,
    min_erts: MIN_ERTS.as_ptr() as *const c_char,
});

#[no_mangle]
pub extern "C" fn crypto_nif_init() -> *const ErlNifEntry {
    &ENTRY.0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
