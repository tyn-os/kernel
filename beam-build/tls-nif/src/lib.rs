//! Tyn in-guest inbound TLS NIF (`tyn_tls`): rustls + pure-Rust RustCrypto,
//! sans-IO. BEAM owns the socket; this NIF only transforms bytes. Backs the
//! `Tyn.Transports.RustlsTLS` ThousandIsland transport.
//!
//! - RNG: getrandom custom backend -> an RDSEED->ChaCha20 CSPRNG (the src/rng.rs
//!   construction). Not raw RDRAND, not the getrandom syscall.
//! - Time: rustls's std TimeProvider -> SystemTime -> clock_gettime, which on Tyn
//!   is the kvmclock-backed CLOCK_REALTIME (cert validity).
//! - Sessions held in a handle table; the transport frees via close/1.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection};
use std::sync::Arc;

// ============================ CSPRNG (src/rng.rs construction) ============================
static CSPRNG: LazyLock<Mutex<ChaCha20Rng>> = LazyLock::new(|| Mutex::new(seed_csprng()));

fn seed_csprng() -> ChaCha20Rng {
    let mut seed = [0u8; 32];
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{__cpuid_count, _rdseed64_step};
        if __cpuid_count(7, 0).ebx & (1 << 18) != 0 {
            for lane in 0..4 {
                let mut acc = 0u64;
                for _ in 0..8 {
                    let mut v = 0u64;
                    for _try in 0..64 {
                        if _rdseed64_step(&mut v) == 1 {
                            break;
                        }
                    }
                    acc ^= v;
                }
                seed[lane * 8..lane * 8 + 8].copy_from_slice(&acc.to_le_bytes());
            }
            return ChaCha20Rng::from_seed(seed);
        }
    }
    // No RDSEED (non-x86 dev host): fall back to getrandom's presence is moot here;
    // use a fixed dev seed. On Tyn/x86 the RDSEED path above always runs.
    for (i, b) in seed.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    ChaCha20Rng::from_seed(seed)
}

#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom::Error> {
    let buf = std::slice::from_raw_parts_mut(dest, len);
    CSPRNG.lock().unwrap().fill_bytes(buf);
    Ok(())
}

// ============================ erl_nif ABI (NIF 2.17, OTP 27) ============================
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
type UpgradeFn = extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, *mut *mut c_void, ErlNifTerm) -> c_int;
type UnloadFn = extern "C" fn(*mut ErlNifEnv, *mut c_void);
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
const DIRTY_CPU: c_uint = 1;

extern "C" {
    fn enif_inspect_iolist_as_binary(env: *mut ErlNifEnv, term: ErlNifTerm, bin: *mut ErlNifBinary) -> c_int;
    fn enif_make_new_binary(env: *mut ErlNifEnv, size: usize, termp: *mut ErlNifTerm) -> *mut u8;
    fn enif_make_atom(env: *mut ErlNifEnv, name: *const c_char) -> ErlNifTerm;
    fn enif_make_badarg(env: *mut ErlNifEnv) -> ErlNifTerm;
    fn enif_make_tuple_from_array(env: *mut ErlNifEnv, arr: *const ErlNifTerm, cnt: c_uint) -> ErlNifTerm;
    fn enif_get_ulong(env: *mut ErlNifEnv, term: ErlNifTerm, ip: *mut c_ulong) -> c_int;
    fn enif_make_ulong(env: *mut ErlNifEnv, x: c_ulong) -> ErlNifTerm;
}

// ---------------------------- marshalling helpers ----------------------------
unsafe fn get_bin<'a>(env: *mut ErlNifEnv, term: ErlNifTerm) -> Option<&'a [u8]> {
    let mut b = ErlNifBinary { size: 0, data: std::ptr::null_mut(), ref_bin: std::ptr::null_mut(), spare: [std::ptr::null_mut(); 2] };
    if enif_inspect_iolist_as_binary(env, term, &mut b) == 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(b.data, b.size))
}
unsafe fn make_bin(env: *mut ErlNifEnv, data: &[u8]) -> ErlNifTerm {
    let mut term: ErlNifTerm = 0;
    let ptr = enif_make_new_binary(env, data.len(), &mut term);
    if !ptr.is_null() && !data.is_empty() {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    }
    term
}
unsafe fn atom(env: *mut ErlNifEnv, name: &str) -> ErlNifTerm {
    let mut buf = [0u8; 32];
    let n = name.len().min(31);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    enif_make_atom(env, buf.as_ptr() as *const c_char)
}
unsafe fn tuple2(env: *mut ErlNifEnv, a: ErlNifTerm, b: ErlNifTerm) -> ErlNifTerm {
    let arr = [a, b];
    enif_make_tuple_from_array(env, arr.as_ptr(), 2)
}
unsafe fn ok_h(env: *mut ErlNifEnv, h: u64) -> ErlNifTerm {
    tuple2(env, atom(env, "ok"), enif_make_ulong(env, h as c_ulong))
}
unsafe fn err(env: *mut ErlNifEnv, msg: &str) -> ErlNifTerm {
    tuple2(env, atom(env, "error"), make_bin(env, msg.as_bytes()))
}
unsafe fn get_h(env: *mut ErlNifEnv, term: ErlNifTerm) -> Option<u64> {
    let mut h: c_ulong = 0;
    if enif_get_ulong(env, term, &mut h) == 0 {
        return None;
    }
    Some(h as u64)
}
macro_rules! badarg {
    ($env:expr) => {
        return enif_make_badarg($env)
    };
}

// ============================ session tables ============================
static CONFIGS: LazyLock<Mutex<HashMap<u64, Arc<ServerConfig>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CONNS: LazyLock<Mutex<HashMap<u64, ServerConnection>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);
fn next_handle() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ============================ NIF functions ============================
// config_new(cert_der :: binary, key_pkcs8_der :: binary) -> {:ok, cfg_h} | {:error, bin}
extern "C" fn nif_config_new(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 2);
        let cert = match get_bin(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let key = match get_bin(env, a[1]) { Some(x) => x, None => badarg!(env) };
        let cert = CertificateDer::from(cert.to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec()));
        let provider = Arc::new(rustls_rustcrypto::provider());
        let cfg = match ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
        {
            Ok(b) => b,
            Err(e) => return err(env, &format!("versions: {e:?}")),
        };
        let cfg = match cfg.with_no_client_auth().with_single_cert(vec![cert], key) {
            Ok(c) => c,
            Err(e) => return err(env, &format!("cert/key: {e:?}")),
        };
        let h = next_handle();
        CONFIGS.lock().unwrap().insert(h, Arc::new(cfg));
        ok_h(env, h)
    }
}

// conn_new(cfg_h) -> {:ok, conn_h} | {:error, bin}
extern "C" fn nif_conn_new(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 1 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 1);
        let cfg_h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let cfg = match CONFIGS.lock().unwrap().get(&cfg_h).cloned() {
            Some(c) => c,
            None => return err(env, "bad config handle"),
        };
        let conn = match ServerConnection::new(cfg) {
            Ok(c) => c,
            Err(e) => return err(env, &format!("conn: {e:?}")),
        };
        let h = next_handle();
        CONNS.lock().unwrap().insert(h, conn);
        ok_h(env, h)
    }
}

fn with_conn<R>(h: u64, f: impl FnOnce(&mut ServerConnection) -> R) -> Option<R> {
    let mut g = CONNS.lock().unwrap();
    g.get_mut(&h).map(f)
}

// feed(conn_h, ciphertext :: binary) -> :ok | {:error, bin}
extern "C" fn nif_feed(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 2);
        let h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let ct = match get_bin(env, a[1]) { Some(x) => x, None => badarg!(env) };
        let r = with_conn(h, |c| {
            let mut cur = ct;
            while !cur.is_empty() {
                match c.read_tls(&mut cur) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => return Err(format!("read_tls: {e}")),
                }
            }
            c.process_new_packets().map(|_| ()).map_err(|e| format!("tls: {e}"))
        });
        match r {
            None => err(env, "bad conn handle"),
            Some(Ok(())) => atom(env, "ok"),
            Some(Err(m)) => err(env, &m),
        }
    }
}

// pull_send(conn_h) -> binary   (ciphertext to write to the socket; may be <<>>)
extern "C" fn nif_pull_send(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 1 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 1);
        let h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let r = with_conn(h, |c| {
            let mut out = Vec::new();
            while c.wants_write() {
                if c.write_tls(&mut out).is_err() {
                    break;
                }
            }
            out
        });
        match r {
            None => make_bin(env, b""),
            Some(out) => make_bin(env, &out),
        }
    }
}

// read_plain(conn_h, n :: uint) -> binary   (n==0 => all available)
extern "C" fn nif_read_plain(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 2);
        let h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let n = match get_h(env, a[1]) { Some(x) => x as usize, None => badarg!(env) };
        let r = with_conn(h, |c| {
            use std::io::Read;
            let mut rdr = c.reader();
            if n == 0 {
                let mut out = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match rdr.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(k) => {
                            out.extend_from_slice(&tmp[..k]);
                            if out.len() >= 262_144 {
                                break;
                            }
                        }
                        Err(_) => break, // WouldBlock: nothing more buffered
                    }
                }
                out
            } else {
                let mut buf = vec![0u8; n];
                match rdr.read(&mut buf) {
                    Ok(k) => {
                        buf.truncate(k);
                        buf
                    }
                    Err(_) => Vec::new(),
                }
            }
        });
        match r {
            None => make_bin(env, b""),
            Some(out) => make_bin(env, &out),
        }
    }
}

// write_plain(conn_h, plaintext :: binary) -> :ok | {:error, bin}
extern "C" fn nif_write_plain(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 2 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 2);
        let h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        let pt = match get_bin(env, a[1]) { Some(x) => x, None => badarg!(env) };
        let r = with_conn(h, |c| {
            use std::io::Write;
            c.writer().write_all(pt).map_err(|e| format!("write: {e}"))
        });
        match r {
            None => err(env, "bad conn handle"),
            Some(Ok(())) => atom(env, "ok"),
            Some(Err(m)) => err(env, &m),
        }
    }
}

// is_handshaking(conn_h) -> true | false
extern "C" fn nif_is_handshaking(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 1 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 1);
        let h = match get_h(env, a[0]) { Some(x) => x, None => badarg!(env) };
        match with_conn(h, |c| c.is_handshaking()) {
            Some(true) | None => atom(env, "true"),
            Some(false) => atom(env, "false"),
        }
    }
}

// close_conn(conn_h) -> :ok
extern "C" fn nif_close_conn(env: *mut ErlNifEnv, argc: c_int, a: *const ErlNifTerm) -> ErlNifTerm {
    unsafe {
        if argc != 1 {
            badarg!(env);
        }
        let a = std::slice::from_raw_parts(a, 1);
        if let Some(h) = get_h(env, a[0]) {
            CONNS.lock().unwrap().remove(&h);
        }
        atom(env, "ok")
    }
}

// ============================ entry ============================
const NAME: &[u8] = b"tyn_tls\0";
const VM_VARIANT: &[u8] = b"beam.vanilla\0";
const MIN_ERTS: &[u8] = b"erts-14.0\0";
const F_CONFIG_NEW: &[u8] = b"config_new\0";
const F_CONN_NEW: &[u8] = b"conn_new\0";
const F_FEED: &[u8] = b"feed\0";
const F_PULL_SEND: &[u8] = b"pull_send\0";
const F_READ_PLAIN: &[u8] = b"read_plain\0";
const F_WRITE_PLAIN: &[u8] = b"write_plain\0";
const F_IS_HS: &[u8] = b"is_handshaking\0";
const F_CLOSE: &[u8] = b"close_conn\0";

struct Sync<T>(T);
unsafe impl<T> std::marker::Sync for Sync<T> {}

static FUNCS: Sync<[ErlNifFunc; 8]> = Sync([
    // config_new + conn_new touch the crypto (keygen validation / ECDHE setup) -> dirty CPU.
    ErlNifFunc { name: F_CONFIG_NEW.as_ptr() as *const c_char, arity: 2, fptr: nif_config_new, flags: DIRTY_CPU },
    ErlNifFunc { name: F_CONN_NEW.as_ptr() as *const c_char, arity: 1, fptr: nif_conn_new, flags: 0 },
    // feed drives the handshake crypto (ECDHE, signatures) -> dirty CPU.
    ErlNifFunc { name: F_FEED.as_ptr() as *const c_char, arity: 2, fptr: nif_feed, flags: DIRTY_CPU },
    ErlNifFunc { name: F_PULL_SEND.as_ptr() as *const c_char, arity: 1, fptr: nif_pull_send, flags: 0 },
    ErlNifFunc { name: F_READ_PLAIN.as_ptr() as *const c_char, arity: 2, fptr: nif_read_plain, flags: 0 },
    ErlNifFunc { name: F_WRITE_PLAIN.as_ptr() as *const c_char, arity: 2, fptr: nif_write_plain, flags: 0 },
    ErlNifFunc { name: F_IS_HS.as_ptr() as *const c_char, arity: 1, fptr: nif_is_handshaking, flags: 0 },
    ErlNifFunc { name: F_CLOSE.as_ptr() as *const c_char, arity: 1, fptr: nif_close_conn, flags: 0 },
]);

static ENTRY: Sync<ErlNifEntry> = Sync(ErlNifEntry {
    major: 2,
    // 2.16 loads on both OTP 25 (local test host) and OTP 27 (Tyn) — forward-compatible.
    minor: 16,
    name: NAME.as_ptr() as *const c_char,
    num_of_funcs: 8,
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

/// Static-link entry (Tyn: nifs/tyn_tls.a -> load_nif("tyn_tls")).
#[no_mangle]
pub extern "C" fn tyn_tls_nif_init() -> *const ErlNifEntry {
    &ENTRY.0
}
/// Dynamic-load entry (local: erlang:load_nif of the .so).
#[no_mangle]
pub extern "C" fn nif_init() -> *const ErlNifEntry {
    &ENTRY.0
}
