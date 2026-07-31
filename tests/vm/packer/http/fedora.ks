text
firstboot --disable
lang en_US.UTF-8
keyboard us
timezone Etc/UTC --utc

network --bootproto=dhcp --device=link --activate --hostname=agentdesktop-vm
url --mirrorlist="https://mirrors.fedoraproject.org/mirrorlist?repo=fedora-44&arch=x86_64"
repo --name=updates --mirrorlist="https://mirrors.fedoraproject.org/mirrorlist?repo=updates-released-f44&arch=x86_64"

rootpw --lock
user --name=agentdesktop --groups=wheel --password=agentdesktop --plaintext
firewall --enabled --service=ssh
selinux --enforcing
services --enabled=sshd,qemu-guest-agent

zerombr
clearpart --all --initlabel
autopart --type=lvm
reboot

%packages
@^workstation-product-environment
curl
git
jq
openssh-server
qemu-guest-agent
%end

%post
cat > /etc/ssh/sshd_config.d/90-agentdesktop-vm.conf <<'EOF'
PasswordAuthentication yes
PermitRootLogin no
EOF

cat >> /etc/hosts <<'EOF'
10.0.2.2 host.internal
10.0.2.100 host.test
EOF

echo 'agentdesktop ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/agentdesktop
chmod 0440 /etc/sudoers.d/agentdesktop
systemctl set-default graphical.target
%end