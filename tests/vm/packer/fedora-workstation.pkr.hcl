packer {
  required_plugins {
    qemu = {
      source  = "github.com/hashicorp/qemu"
      version = ">= 1.0.10"
    }
  }
}

variable "accelerator" {
  type    = string
  default = "kvm"
}

variable "cpus" {
  type    = number
  default = 4
}

variable "claude_code_version" {
  type    = string
  default = "2.1.212"
}

variable "disk_size" {
  type    = string
  default = "50G"
}

variable "headless" {
  type    = bool
  default = true
}

variable "iso_checksum" {
  type    = string
  default = "sha256:bd285201494dd0ba09b54d05ac707de1401668b8512a573edb5922dcf9d7067e"
}

variable "iso_url" {
  type    = string
  default = "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Everything/x86_64/iso/Fedora-Everything-netinst-x86_64-44-1.7.iso"
}

variable "memory" {
  type    = number
  default = 8192
}

source "qemu" "fedora_workstation" {
  accelerator      = var.accelerator
  boot_command     = ["e<wait>", "<down><down><end>", " inst.text inst.ks=http://{{ .HTTPIP }}:{{ .HTTPPort }}/fedora.ks", "<leftCtrlOn>x<leftCtrlOff>"]
  boot_wait        = "10s"
  cpus             = var.cpus
  disk_compression = true
  disk_interface   = "virtio"
  disk_size        = var.disk_size
  format           = "qcow2"
  headless         = var.headless
  http_directory   = "${path.root}/http"
  iso_checksum     = var.iso_checksum
  iso_url          = var.iso_url
  memory           = var.memory
  net_device       = "virtio-net"
  output_directory = "${path.root}/../.artifacts/base"
  shutdown_command = "echo agentedge | sudo -S systemctl poweroff"
  ssh_password     = "agentedge"
  ssh_timeout      = "45m"
  ssh_username     = "agentedge"
  vm_name          = "fedora-workstation-base.qcow2"
}

build {
  sources = ["source.qemu.fedora_workstation"]

  provisioner "shell" {
    inline = [
      "sudo dnf install -y nodejs npm",
      "sudo npm install --global --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code@${var.claude_code_version}",
      "claude --version | grep -F '${var.claude_code_version}'",
      "sudo npm cache clean --force",
    ]
  }
}
