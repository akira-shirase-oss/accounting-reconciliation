#!/usr/bin/env bash
# A private certificate authority and a server certificate for the TLS test database
# (`postgres-tls` in deploy/compose.yaml, D-070). Development and CI only; nothing here is a
# secret and nothing is committed (`.dev-tls/` is ignored).
#
# The server certificate names `localhost` only (no IP address), so the tests can prove
# both halves of verify-full against one server: `localhost` with this CA connects;
# `127.0.0.1` (a name the certificate does not carry) and an unknown CA are refused.
#
#   scripts/dev-tls.sh        create .dev-tls/{ca.pem,server.pem,server.key} if missing
set -euo pipefail
cd "$(dirname "$0")/.."

dir=.dev-tls
if [ -s "$dir/ca.pem" ] && [ -s "$dir/server.pem" ] && [ -s "$dir/server.key" ]; then
  exit 0
fi
mkdir -p "$dir"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat >"$work/ca.cnf" <<'CNF'
[req]
distinguished_name = dn
prompt = no
x509_extensions = v3_ca
[dn]
CN = Accounting Reconciliation development CA
[v3_ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
CNF
cat >"$work/server.ext" <<'CNF'
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
CNF

openssl ecparam -name prime256v1 -genkey -noout -out "$work/ca.key"
openssl req -x509 -new -key "$work/ca.key" -sha256 -days 825 -config "$work/ca.cnf" -out "$work/ca.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/server.key"
openssl req -new -key "$work/server.key" -subj "/CN=localhost" -out "$work/server.csr"
openssl x509 -req -in "$work/server.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" \
  -CAcreateserial -sha256 -days 825 -extfile "$work/server.ext" -out "$work/server.pem" 2>/dev/null
# PKCS#8, which both PostgreSQL and rustls read.
openssl pkcs8 -topk8 -nocrypt -in "$work/server.key" -out "$dir/server.key"
chmod 0644 "$dir/server.key"   # copied into the container as postgres, mode 0600
cp "$work/ca.pem" "$work/server.pem" "$dir/"
echo "dev TLS ok: $dir (CA, server certificate for localhost)"
