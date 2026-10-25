# tier-0 test node: systemd + sshd, so kurin can really log in and run tasks.
# Built once by up.sh (tag kurin/tier0-node:local); no network is needed at test
# time beyond the initial base-image pull.
FROM jrei/systemd-ubuntu:22.04

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /root/.ssh \
 && chmod 700 /root/.ssh \
 && systemctl enable ssh

EXPOSE 22
CMD ["/lib/systemd/systemd"]