set -e
K=~/kernel; OTP=~/.asdf/installs/erlang/27.3.4.2; L=$OTP/lib
REL=~/l2app/_build/prod/rel/l2app
echo "=== 1. tyn-pack l2app (base) ==="
"$K/tyn-pack" "$REL" --base "$K/src/otp-rootfs.cpio" -o /tmp/base.cpio >/tmp/pack.log 2>&1 || { tail -5 /tmp/pack.log; exit 1; }
echo "=== 2. extract + clean the crypto/ssl/asn1 module set ==="
rm -rf /tmp/stage && mkdir -p /tmp/stage && cd /tmp/stage && cpio -idm < /tmp/base.cpio 2>/dev/null
# remove release lib duplicates + flat stubs for the TLS/crypto apps
rm -rf lib/ssl-* lib/public_key-* lib/asn1-* lib/crypto-* 2>/dev/null || true
rm -f ssl.app public_key.app asn1.app 2>/dev/null || true
# install the CLEAN OTP-27 set (real beams + real .app) at the flat root
for app in ssl-* public_key-* asn1-* crypto-*; do :; done
SSLD=$(ls -d $L/ssl-* | sort -V | tail -1)
PKD=$(ls -d $L/public_key-* | sort -V | tail -1)
A1D=$(ls -d $L/asn1-* | sort -V | tail -1)
CRD=$(ls -d $L/crypto-* | sort -V | tail -1)
echo "  ssl=$SSLD  pk=$PKD  asn1=$A1D  crypto=$CRD"
cp "$SSLD"/ebin/*.beam "$SSLD"/ebin/*.app .
cp "$PKD"/ebin/*.beam "$PKD"/ebin/*.app .
cp "$A1D"/ebin/*.beam "$A1D"/ebin/*.app .
cp "$CRD"/ebin/*.beam "$CRD"/ebin/*.app .
# overlay MY shims (verify crypto + pure-Erlang asn1rt_nif) — shadow the stock ones
cp "$K"/src/erl/crypto.beam "$K"/src/erl/asn1rt_nif.beam .
echo "=== 3. strip stale lib code_paths for these apps from boot.config ==="
sed -i -E 's#"lib/(ssl|public_key|asn1|crypto)-[^"]*/ebin", ?##g' boot.config
cp /etc/ssl/certs/ca-certificates.crt /tmp/stage/ca-certificates.crt; echo "  CA bundle -> cpio root ($(stat -c%s /tmp/stage/ca-certificates.crt) bytes)"
echo "=== 4. repack ==="
find . | cpio -o -H newc 2>/dev/null > /tmp/aprime.cpio
echo "aprime.cpio: $(stat -c%s /tmp/aprime.cpio) bytes"
echo "=== 5. HOST-VALIDATE module resolution + verify (A-prime beam, flat root as code path) ==="
BEAM=$OTP/erts-15.2.7.1/bin/beam.smp; cp ~/work/beam_out.smp $BEAM
cat > /tmp/res_check.erl <<'EOF'
-module(res_check).
-export([run/1]).
run([Dir]) ->
  code:add_patha(Dir),
  {ok,_}=application:ensure_all_started(ssl),
  io:format("[res] crypto.beam = ~s~n",[code:which(crypto)]),
  io:format("[res] asn1rt_nif  = ~s~n",[code:which(asn1rt_nif)]),
  io:format("[res] ssl         = ~s~n",[code:which(ssl)]),
  io:format("[res] supports pk = ~p~n",[proplists:get_value(public_keys, crypto:supports())]),
  {Pub,_}=crypto:generate_key(ecdh,x25519),
  io:format("[res] ecdhe x25519 pub bytes = ~p~n",[byte_size(Pub)]),
  R=(catch ssl:connect("example.com",443,[{verify,verify_peer},{cacertfile,"/etc/ssl/certs/ca-certificates.crt"},{versions,['tlsv1.3']},{active,false}],15000)),
  case R of {ok,S} -> io:format("[res] ssl:connect OK, cert VERIFIED~n"), ssl:close(S), io:format("RES: PASS~n");
            _ -> io:format("[res] ssl:connect -> ~P~n",[R,6]), io:format("RES: FAIL~n") end,
  halt(0).
EOF
$OTP/bin/erlc -o /tmp /tmp/res_check.erl 2>&1 | head
$OTP/bin/erl -noshell -pa /tmp -run res_check run /tmp/stage 2>&1 | grep -aE "\[res\]|RES:" | head -12
cp ~/work/beam.asdf.orig $BEAM
