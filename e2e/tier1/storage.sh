#!/usr/bin/env bash
# tier-1 storage: [storage] kind=local-path|nfs on a single node.
# Asserts the StorageClass exists (and is default), a PVC binds, and a pod
# can actually write to the volume.
set -euo pipefail

KIND=${1:-local-path}
case "$KIND" in local-path|nfs) ;; *) echo "usage: storage.sh <local-path|nfs>"; exit 1;; esac
NAME=kurin-tier1
CFG=e2e/tier1/config.toml
BIN=target/tier1st/debug/kurin
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

e2e/tier1/up.sh

STORAGE="[storage]
kind = \"$KIND\"
default = true"
if [ "$KIND" = nfs ]; then
    # A loopback NFS export: the provisioner talks to the same node.
    multipass exec "$NAME" -- sudo bash -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq && apt-get install -y -qq nfs-kernel-server >/dev/null
        mkdir -p /srv/nfs-kurin && chmod 0777 /srv/nfs-kurin
        echo "/srv/nfs-kurin 10.97.0.0/16(rw,sync,no_subtree_check,no_root_squash)" > /etc/exports
        systemctl enable --now nfs-server && exportfs -ra'
    IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
    STORAGE="[storage]
kind = \"nfs\"
server = \"$IP\"
path = \"/srv/nfs-kurin\"
default = true"
fi

# keep only a single control-plane section from the generated config
awk '/^\[nodes\./{exit} {print}' "$CFG" > "$TMPCFG"
printf '\n%s\n\n[nodes.node01]\naddress = "%s"\ninternal-address = "%s"\nrole = "control-plane"\n' \
    "$STORAGE" \
    "$(awk -F'"' '/^address = /{print $2; exit}' "$CFG")" \
    "$(awk -F'"' '/^internal-address = /{print $2; exit}' "$CFG")" >> "$TMPCFG"
grep -q "^kind = \"$KIND\"" "$TMPCFG" || { echo "[FAIL] storage injection"; exit 1; }

export PATH="$HOME/.cargo/bin:$PATH"
CARGO_TARGET_DIR=target/tier1st cargo build -p kurin
echo "[storage:$KIND] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2>/tmp/storage-e2e-err.log; then
    echo "[FAIL] create exited nonzero"; tail -5 /tmp/storage-e2e-err.log; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
grep -q '"id":"storage/install@node01"' "$EVENTS" || { echo "[FAIL] no storage task"; exit 1; }
echo "[ok] cluster ready with storage/install"

CLASS="kurin-$KIND"
for i in $(seq 1 30); do
    GOT=$(multipass exec "$NAME" -- $KC get storageclass "$CLASS" -o jsonpath='{.metadata.name}' 2>/dev/null || true)
    [ "$GOT" = "$CLASS" ] && break; sleep 5
done
[ "$GOT" = "$CLASS" ] || { echo "[FAIL] StorageClass $CLASS missing"; exit 1; }
DEFAULT=$(multipass exec "$NAME" -- $KC get storageclass "$CLASS" -o jsonpath='{.metadata.annotations.storageclass\.kubernetes\.io/is-default-class}' 2>/dev/null || true)
[ "$DEFAULT" = "true" ] || { echo "[FAIL] StorageClass not default (got '$DEFAULT')"; exit 1; }
echo "[ok] StorageClass $CLASS present and default"

cat > "$HOME/kurin-pvc-$KIND.yaml" <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: kurin-e2e
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 64Mi
---
apiVersion: v1
kind: Pod
metadata:
  name: kurin-e2e-writer
spec:
  restartPolicy: Never
  tolerations:
  - key: node-role.kubernetes.io/control-plane
    operator: Exists
    effect: NoSchedule
  containers:
  - name: w
    image: busybox:1.36.1
    imagePullPolicy: IfNotPresent
    command: ["sh", "-c", "echo hello-storage > /data/probe && sync && cat /data/probe"]
    volumeMounts:
    - {name: v, mountPath: /data}
  volumes:
  - name: v
    persistentVolumeClaim:
      claimName: kurin-e2e
EOF
multipass transfer "$HOME/kurin-pvc-$KIND.yaml" "$NAME":/tmp/kurin-pvc.yaml
rm -f "$HOME/kurin-pvc-$KIND.yaml"
multipass exec "$NAME" -- sudo kubectl --kubeconfig /etc/kubernetes/admin.conf apply -f /tmp/kurin-pvc.yaml >/dev/null

for i in $(seq 1 40); do
    PHASE=$(multipass exec "$NAME" -- $KC get pvc kurin-e2e -o jsonpath='{.status.phase}' 2>/dev/null || true)
    [ "$PHASE" = "Bound" ] && break; sleep 5
done
[ "$PHASE" = "Bound" ] || { echo "[FAIL] PVC phase=$PHASE"; multipass exec "$NAME" -- $KC describe pvc kurin-e2e | tail -8; exit 1; }
echo "[ok] PVC bound"

for i in $(seq 1 40); do
    LOG=$(multipass exec "$NAME" -- $KC logs kurin-e2e-writer 2>/dev/null || true)
    [ "$LOG" = "hello-storage" ] && break; sleep 5
done
[ "$LOG" = "hello-storage" ] || { echo "[FAIL] writer pod log='$LOG'"; multipass exec "$NAME" -- $KC describe pod kurin-e2e-writer | tail -10; exit 1; }
echo "[ok] pod wrote and read back through the provisioned volume"
echo "tier1-storage-$KIND: PASS"