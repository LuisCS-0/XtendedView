"""Certificados HTTPS locales con la CLI de openssl.

- Una autoridad (CA) propia que se crea una sola vez y se instala en cada dispositivo.
- Un certificado de servidor firmado por esa CA con las IP actuales del portátil.
  Se vuelve a emitir si aparece una IP nueva (por ejemplo, al activar el hotspot).

La CA está limitada a direcciones privadas y a localhost (igual que en el host en Rust): aunque
alguien robara su clave, no serviría para suplantar webs de internet en los dispositivos.
"""
import ipaddress
import os
import secrets
import shutil
import socket
import subprocess
from pathlib import Path

OPENSSL_CANDIDATES = [r"C:\Program Files\OpenSSL-Win64\bin\openssl.exe",
                      r"C:\Program Files\Git\usr\bin\openssl.exe"]
# IP habitual del portátil cuando comparte su propia red Wi-Fi (zona con cobertura / Wi-Fi Direct).
HOTSPOT_IPS = ["192.168.137.1"]

# Nombre de la CA en la lista de certificados del dispositivo (le sigue un identificador al azar).
CA_NAME = "XtendedView - CA local"
# Redes para las que vale la CA (ninguna existe en internet). Igual que PERMITTED_V4 en Rust.
PERMITTED_NETS = [ipaddress.ip_network(n) for n in (
    "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "169.254.0.0/16", "127.0.0.0/8")]
NAME_CONSTRAINTS = ",".join(
    ["critical"]
    + [f"permitted;IP:{n.network_address}/{n.netmask}" for n in PERMITTED_NETS]
    + ["permitted;DNS:localhost", "excluded;IP:::/::"])  # IPv6 entero (máscara en formato dirección)

CONFIG = """\
[req]
distinguished_name = dn
prompt = no
[dn]
CN = {ca_name} {ca_id}
[v3_ca]
basicConstraints = critical,CA:TRUE,pathlen:0
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
nameConstraints = {constraints}
[v3_leaf]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = serverAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
subjectAltName = @alt
[alt]
{alt}
"""


def find_openssl() -> str:
    for c in [shutil.which("openssl"), *OPENSSL_CANDIDATES]:
        if c and Path(c).is_file():
            return c
    raise FileNotFoundError("No encuentro openssl (necesario para el certificado HTTPS).")


def local_ipv4s() -> list[str]:
    ips = set()
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            ips.add(info[4][0])
    except socket.gaierror:
        pass
    return sorted(ips, key=ipaddress.ip_address)


def usable_ips() -> list[str]:
    """IP a las que tiene sentido conectarse desde otro dispositivo."""
    return [ip for ip in local_ipv4s()
            if not ipaddress.ip_address(ip).is_loopback
            and not ipaddress.ip_address(ip).is_link_local]


class CertManager:
    def __init__(self, directory: Path):
        self.dir = directory
        self.dir.mkdir(parents=True, exist_ok=True)
        self.openssl = find_openssl()
        self.ca_crt, self.ca_key = self.dir / "ca.crt", self.dir / "ca.key"
        self.crt, self.key = self.dir / "server.crt", self.dir / "server.key"
        self.ips_file = self.dir / "server.ips"
        self.host = socket.gethostname()

    def _run(self, *args: str) -> None:
        r = subprocess.run([self.openssl, *args], capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError(f"openssl {args[0]} falló:\n{r.stderr}")

    def _config(self, ips: list[str]) -> Path:
        # Solo direcciones y localhost: ni el nombre del equipo (se ve al conectarse) ni otros
        # nombres, que la CA no permite.
        alt = ["DNS.1 = localhost"]
        alt += [f"IP.{i + 1} = {ip}" for i, ip in enumerate(ips)]
        ca_id = "".join(secrets.choice("ABCDEFGHJKMNPQRSTUVWXYZ23456789") for _ in range(4))
        path = self.dir / "openssl.cnf"
        path.write_text(CONFIG.format(ca_name=CA_NAME, ca_id=ca_id, constraints=NAME_CONSTRAINTS,
                                      alt="\n".join(alt)), encoding="ascii")
        return path

    def wanted_ips(self) -> list[str]:
        ips = set(local_ipv4s()) | {"127.0.0.1", *HOTSPOT_IPS}
        # Una IP fuera de las redes permitidas invalidaría el certificado entero.
        ips = {ip for ip in ips if any(ipaddress.ip_address(ip) in n for n in PERMITTED_NETS)}
        return sorted(ips, key=ipaddress.ip_address)

    def _ca_is_current(self) -> bool:
        """La CA guardada es la actual: con las restricciones y el nombre de XtendedView."""
        r = subprocess.run([self.openssl, "x509", "-in", str(self.ca_crt), "-noout", "-text"],
                           capture_output=True, text=True)
        return r.returncode == 0 and "Name Constraints" in r.stdout and CA_NAME in r.stdout

    def ensure(self) -> bool:
        """Crea lo que falte. Devuelve True si el certificado del servidor ha cambiado."""
        ips = self.wanted_ips()
        exists = self.ca_crt.exists() and self.ca_key.exists()
        if not exists or not self._ca_is_current():
            if exists:
                print("Se rehace la CA del ordenador (ahora limitada a direcciones privadas). "
                      "Hay que volver a instalarla en los dispositivos y borrar la anterior.")
            cnf = self._config(ips)
            self._run("req", "-x509", "-new", "-config", str(cnf), "-extensions", "v3_ca",
                      "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                      "-keyout", str(self.ca_key), "-out", str(self.ca_crt), "-days", "3650")
            self.crt.unlink(missing_ok=True)
            self.ips_file.unlink(missing_ok=True)
        current = self.ips_file.read_text().split() if self.ips_file.exists() else []
        current = [ip for ip in current if any(ipaddress.ip_address(ip) in n for n in PERMITTED_NETS)]
        if self.crt.exists() and self.key.exists() and set(ips) <= set(current):
            return False
        ips = sorted(set(ips) | set(current), key=ipaddress.ip_address)
        cnf = self._config(ips)
        csr = self.dir / "server.csr"
        self._run("req", "-new", "-config", str(cnf), "-newkey", "ec",
                  "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-subj", "/CN=XtendedView",
                  "-keyout", str(self.key), "-out", str(csr))
        self._run("x509", "-req", "-in", str(csr), "-CA", str(self.ca_crt),
                  "-CAkey", str(self.ca_key), "-set_serial", "0x" + secrets.token_hex(12),
                  "-days", "397", "-extfile", str(cnf), "-extensions", "v3_leaf",
                  "-out", str(self.crt))
        csr.unlink(missing_ok=True)
        self.ips_file.write_text(" ".join(ips))
        return True

    def fingerprint(self) -> str:
        r = subprocess.run([self.openssl, "x509", "-in", str(self.ca_crt), "-noout",
                            "-fingerprint", "-sha256"], capture_output=True, text=True)
        return r.stdout.strip().split("=", 1)[-1]


if __name__ == "__main__":
    m = CertManager(Path(os.environ.get("CERT_DIR", "certs")))
    print("cambiado:", m.ensure(), "| IP:", m.ips_file.read_text())
