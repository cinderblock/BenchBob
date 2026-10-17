#!/bin/bash
set -euo pipefail

# DeskyPi Installer
# Sets up a Raspberry Pi as a USB drive server with NFS, NBD, Samba, and a web dashboard.

INSTALL_DIR="/opt/deskypi"
SERVICE_USER="pi"

red()   { echo -e "\033[1;31m$*\033[0m"; }
green() { echo -e "\033[1;32m$*\033[0m"; }
blue()  { echo -e "\033[1;34m$*\033[0m"; }

if [ "$EUID" -ne 0 ]; then
  red "Please run as root: sudo ./install.sh"
  exit 1
fi

blue "=== DeskyPi Installer ==="

# ---------- 1. Dependencies ----------
blue "Installing system packages..."
apt-get update -qq
apt-get install -y -qq \
  nfs-kernel-server \
  nbd-server \
  samba \
  dkms \
  git \
  apfsprogs \
  rpiboot \
  rpi-eeprom \
  > /dev/null

# Peripherals hub: the tools each driver in peripherals/drivers/ drives. Several of these
# ship a daemon that would otherwise start at boot and hold devices or ports; DeskyPi runs
# them itself, on demand, as child processes (step 9b masks the units).
apt-get install -y -qq \
  iw \
  i2c-tools \
  can-utils \
  usbip \
  uhubctl \
  ustreamer \
  v4l-utils \
  lldpd \
  iperf3 \
  ethtool \
  bluez \
  rtl-sdr \
  rtl-433 \
  gpsd \
  gpsd-clients \
  nut-server \
  nut-client \
  openocd \
  esptool \
  avrdude \
  udhcpc \
  python3-serial \
  > /dev/null

# Install Bun if not present
if ! command -v bun &>/dev/null; then
  blue "Installing Bun..."
  su - "$SERVICE_USER" -c 'curl -fsSL https://bun.sh/install | bash' > /dev/null
fi

BUN_PATH=$(su - "$SERVICE_USER" -c 'which bun')

# ---------- 2. Directory structure ----------
blue "Setting up directories..."
mkdir -p /srv/nfs/drives
mkdir -p /etc/nbd-server/conf.d
mkdir -p "$INSTALL_DIR"

# ---------- 3. Application files ----------
blue "Installing application files..."
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cp "$SCRIPT_DIR"/*.ts "$INSTALL_DIR/"
cp "$SCRIPT_DIR/index.html" "$INSTALL_DIR/"
rm -rf "$INSTALL_DIR/peripherals" "$INSTALL_DIR/ui"
cp -r "$SCRIPT_DIR/peripherals" "$SCRIPT_DIR/ui" "$INSTALL_DIR/"

if [ ! -f "$INSTALL_DIR/defaults.json" ]; then
  cat > "$INSTALL_DIR/defaults.json" << 'DEFAULTS'
{
  "hostname": "",
  "username": "pi",
  "enableSsh": true,
  "sshKeys": [],
  "wifiNetworks": []
}
DEFAULTS
fi
chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

# ---------- 4. Drive handler scripts ----------
blue "Installing drive handler scripts..."
cp "$SCRIPT_DIR/nfs-drive-handler.sh" /usr/local/bin/
cp "$SCRIPT_DIR/nfs-drive-mount.sh" /usr/local/bin/
chmod +x /usr/local/bin/nfs-drive-handler.sh
chmod +x /usr/local/bin/nfs-drive-mount.sh

# ---------- 5. Udev rules ----------
blue "Installing udev rules..."
cp "$SCRIPT_DIR/90-nfs-drive-export.rules" /etc/udev/rules.d/
udevadm control --reload-rules

# ---------- 6. NFS exports ----------
if ! grep -q '/srv/nfs' /etc/exports 2>/dev/null; then
  blue "Configuring NFS exports..."
  echo '/srv/nfs  *(ro,fsid=0,no_subtree_check,root_squash,crossmnt)' >> /etc/exports
fi

# ---------- 7. NBD server ----------
if ! grep -q 'includedir = /etc/nbd-server/conf.d' /etc/nbd-server/config 2>/dev/null; then
  blue "Configuring NBD server..."
  cat > /etc/nbd-server/config << 'NBD'
[generic]
        allowlist = true
	user = root
	group = root
	includedir = /etc/nbd-server/conf.d
NBD
fi

# ---------- 8. Samba ----------
if ! grep -q '\[drives\]' /etc/samba/smb.conf 2>/dev/null; then
  blue "Configuring Samba share..."
  cat >> /etc/samba/smb.conf << 'SMB'

[drives]
   comment = USB Drives
   path = /srv/nfs/drives
   browseable = yes
   read only = yes
   guest ok = yes
   force user = root
SMB
fi

# ---------- 9. APFS module ----------
blue "Configuring APFS support..."
# Debian's apfs-dkms (0.3.13) stops building at kernel 6.18 (`struct page` lost `index`),
# which leaves a kernel upgrade half-configured. Build the pinned upstream release under
# DKMS instead, so every future kernel gets the module rebuilt automatically.
APFS_VER=0.3.22
if ! dkms status "linux-apfs-rw/$APFS_VER" 2>/dev/null | grep -q installed; then
  dpkg -s apfs-dkms &>/dev/null && dpkg -r apfs-dkms > /dev/null
  rm -rf "/usr/src/linux-apfs-rw-$APFS_VER"
  git clone -q --depth 1 -b "v$APFS_VER" https://github.com/linux-apfs/linux-apfs-rw "/usr/src/linux-apfs-rw-$APFS_VER"
  rm -rf "/usr/src/linux-apfs-rw-$APFS_VER/.git"
  dkms add "linux-apfs-rw/$APFS_VER" > /dev/null
  for k in /lib/modules/*/build; do
    dkms install "linux-apfs-rw/$APFS_VER" -k "$(basename "$(dirname "$k")")" > /dev/null
  done
fi
apt-mark manual dkms > /dev/null
echo "apfs" > /etc/modules-load.d/apfs.conf
modprobe apfs 2>/dev/null || true

# ---------- 9b. Peripherals ----------
blue "Configuring peripherals..."
# On-demand only: DeskyPi starts these with private configs when a panel needs them.
for unit in lldpd iperf3 gpsd gpsd.socket nut-server nut-monitor nut-driver-enumerator nut-driver.target nut.target; do
  systemctl disable --now "$unit" &>/dev/null || true
  systemctl mask "$unit" &>/dev/null || true
done
# /dev/i2c-* nodes for the I2C panel. Enabling the header buses themselves (I2C, SPI, UART,
# 1-Wire) is a config.txt change the GPIO panel stages with a diff, so it isn't forced here.
echo "i2c-dev" > /etc/modules-load.d/i2c-dev.conf
modprobe i2c-dev 2>/dev/null || true
# Debian's rtl-sdr package ships no blacklist, so the kernel's DVB-T TV driver would claim
# an RTL-SDR stick before rtl_tcp/rtl_433 can open it.
printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n' > /etc/modprobe.d/blacklist-rtl-sdr.conf

# ---------- 10. Systemd service ----------
blue "Installing systemd service..."
cat > /etc/systemd/system/deskypi-web.service << EOF
[Unit]
Description=DeskyPi Web Interface
After=network.target nfs-server.service nbd-server.service

[Service]
Type=simple
User=$SERVICE_USER
WorkingDirectory=$INSTALL_DIR
ExecStart=$BUN_PATH run server.ts
Restart=always
RestartSec=5
Environment=PATH=$(dirname "$BUN_PATH"):/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

[Install]
WantedBy=multi-user.target
EOF

# ---------- 11. Enable and start ----------
blue "Enabling services..."
systemctl daemon-reload
systemctl enable --now nfs-kernel-server
systemctl enable --now nbd-server
systemctl enable --now smbd nmbd
systemctl enable --now deskypi-web.service

green "=== DeskyPi installed ==="
green "Web dashboard: http://$(hostname).local"
green "Config file:   $INSTALL_DIR/defaults.json"
green ""
green "Plug in a USB drive to get started."
