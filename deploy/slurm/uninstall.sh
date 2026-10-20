#!/usr/bin/env bash
# Remove Slurm + MUNGE (doum's optional slurm backend). Config backups in /etc/slurm/backup-* are kept unless you delete them.
#     sudo bash deploy/slurm/uninstall.sh
set -euo pipefail
[ "$(id -u)" -eq 0 ] || exec sudo bash "$0" "$@"
systemctl disable --now slurmd slurmctld munge 2>/dev/null || true
DEBIAN_FRONTEND=noninteractive apt-get remove -y slurm-wlm slurmd slurmctld slurm-client munge
echo "Removed. Left in place: /etc/slurm (incl. backups), /var/log/slurm, /var/spool/slurm*."
