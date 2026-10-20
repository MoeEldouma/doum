#!/usr/bin/env bash
# Install Slurm (single node) + MUNGE and load doum's Slurm config (optional `slurm` backend). Run once:
#     sudo bash deploy/slurm/install.sh
# Safe to re-run (it only re-applies the config). Touches no GPU and no running app.
# Undo: sudo bash deploy/slurm/uninstall.sh
set -euo pipefail
[ "$(id -u)" -eq 0 ] || exec sudo bash "$0" "$@"
HERE="$(cd "$(dirname "$0")" && pwd)"
HOST="$(hostname -s)"
USER_NAME="${SUDO_USER:-$(logname 2>/dev/null || id -un)}"
say() { printf '\n\033[1;33m== %s\033[0m\n' "$*"; }

say "1/6 packages: munge + slurm-wlm (Ubuntu 24.04 repository, Slurm 23.11)"
DEBIAN_FRONTEND=noninteractive apt-get install -y munge slurm-wlm

say "2/6 MUNGE (Slurm's local authentication)"
if [ ! -s /etc/munge/munge.key ]; then mungekey --create --keyfile=/etc/munge/munge.key; fi
chown munge:munge /etc/munge/munge.key && chmod 0400 /etc/munge/munge.key
systemctl enable --now munge
munge -n | unmunge >/dev/null && echo "munge OK"

say "3/6 directories"
install -d -o slurm -g slurm -m 0755 /var/spool/slurmctld /var/log/slurm
install -d -o root  -g root  -m 0755 /var/spool/slurmd /etc/slurm

say "4/6 config (backup of any previous one in /etc/slurm/backup-<time>)"
if ls /etc/slurm/*.conf >/dev/null 2>&1; then
  b="/etc/slurm/backup-$(date +%Y%m%d-%H%M%S)"; mkdir -p "$b"; cp -a /etc/slurm/*.conf "$b/"; echo "backed up to $b"
fi
node_hw="$(slurmd -C | head -1)"          # e.g. NodeName=myhost CPUs=20 ... RealMemory=124000
echo "detected: $node_hw"
sed -e "s|@HOST@|${HOST}|g" -e "s|^NodeName=.*|${node_hw} Gres=gpu:1,shard:110 State=UNKNOWN|" "$HERE/slurm.conf" > /etc/slurm/slurm.conf
sed "s|@HOST@|${HOST}|g" "$HERE/gres.conf" > /etc/slurm/gres.conf
cp "$HERE/cgroup.conf" /etc/slurm/
chmod 0644 /etc/slurm/*.conf

say "5/6 services"
systemctl enable slurmctld slurmd
systemctl restart slurmctld; sleep 2; systemctl restart slurmd; sleep 3
scontrol update NodeName="$(hostname -s)" State=RESUME >/dev/null 2>&1 || true

say "6/6 checks"
sinfo
scontrol show node "$(hostname -s)" | grep -E "State=|Gres=|CfgTRES"
echo "test job as $USER_NAME (1 shard, no GPU work):"
sudo -u "$USER_NAME" srun --partition=batch --gres=shard:1 --time=1 bash -c 'echo "  ran as job $SLURM_JOB_ID on $(hostname -s)"'
say "Slurm is installed and configured with GPU shards. Note: doum does not submit jobs to Slurm yet (preview); see docs/how-it-works.md."
