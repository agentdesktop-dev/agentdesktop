packer {
  required_plugins {
    qemu = {
      source  = "github.com/hashicorp/qemu"
      version = "1.1.3"
    }
  }
}

variable "accelerator" {
  type    = string
  default = "kvm"
}

variable "cpu_model" {
  type    = string
  default = "host"
}

variable "iso_checksum" {
  type = string
}

variable "iso_url" {
  type = string
}

source "qemu" "windows_11_enterprise" {
  accelerator              = var.accelerator
  boot_command             = ["<spacebar><wait><spacebar><wait><spacebar>"]
  boot_wait                = "1s"
  cdrom_interface          = "none"
  communicator             = "ssh"
  cpu_model                = var.cpu_model
  cpus                     = 4
  disk_compression         = true
  disk_interface           = "ide"
  disk_size                = "64G"
  efi_boot                 = true
  efi_drop_efivars         = false
  floppy_files             = ["${path.root}/Autounattend.xml", "${path.root}/bootstrap.ps1"]
  format                   = "qcow2"
  headless                 = true
  iso_checksum             = var.iso_checksum
  iso_url                  = var.iso_url
  machine_type             = "q35"
  memory                   = 8192
  net_device               = "e1000"
  output_directory         = "${path.root}/../.artifacts/base"
  qemuargs                 = [["-device", "ide-cd,drive=cdrom0,bus=ide.1,unit=0,bootindex=1"]]
  shutdown_command         = "shutdown.exe /s /t 0"
  shutdown_timeout         = "10m"
  ssh_file_transfer_method = "sftp"
  ssh_password             = "agentdesktop"
  ssh_timeout              = "90m"
  ssh_username             = "agentdesktop"
  vm_name                  = "windows-11-enterprise-base.qcow2"
}

build {
  sources = ["source.qemu.windows_11_enterprise"]
}