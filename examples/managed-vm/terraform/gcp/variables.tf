variable "project_id" {
  description = "Google Cloud project in which to create the development deployment."
  type        = string

  validation {
    condition     = length(trimspace(var.project_id)) > 0
    error_message = "project_id must not be empty."
  }
}

variable "zone" {
  description = "Compute Engine zone for the VM. The VPC subnet and static address use its region."
  type        = string
  default     = "us-central1-a"

  validation {
    condition     = can(regex("^[a-z]+-[a-z0-9]+[0-9]-[a-z]$", var.zone))
    error_message = "zone must be a Compute Engine zone such as us-central1-a."
  }
}

variable "instance_name" {
  description = "Name used for the VM and its GCP networking resources."
  type        = string
  default     = "agentdesktop-managed"

  validation {
    condition     = can(regex("^[a-z]([-a-z0-9]{0,38}[a-z0-9])?$", var.instance_name))
    error_message = "instance_name must be a lowercase GCP resource name no longer than 40 characters."
  }
}

variable "public_host" {
  description = "Public DNS hostname used in OAuth URLs, certificates, and the client bootstrap."
  type        = string

  validation {
    condition = (
      can(regex("^[A-Za-z0-9][A-Za-z0-9.-]*[A-Za-z0-9]$", var.public_host)) &&
      strcontains(var.public_host, ".") &&
      !endswith(lower(var.public_host), ".local") &&
      !endswith(lower(var.public_host), ".localhost")
    )
    error_message = "public_host must be a remote DNS hostname and must not use .local or .localhost."
  }
}

variable "client_source_ranges" {
  description = "IPv4 CIDR ranges allowed to reach OAuth, enrollment, administration, and Agent Gateway."
  type        = list(string)

  validation {
    condition     = length(var.client_source_ranges) > 0 && alltrue([for cidr in var.client_source_ranges : can(cidrnetmask(cidr))])
    error_message = "client_source_ranges must contain at least one valid IPv4 CIDR."
  }
}

variable "dns_managed_zone" {
  description = "Optional existing Cloud DNS managed-zone name. When set, Terraform creates the public_host A record."
  type        = string
  default     = null
  nullable    = true
}

variable "machine_type" {
  description = "Compute Engine machine type."
  type        = string
  default     = "e2-standard-4"
}

variable "boot_disk_size_gb" {
  description = "Ubuntu boot disk size in GiB."
  type        = number
  default     = 30

  validation {
    condition     = var.boot_disk_size_gb >= 30
    error_message = "boot_disk_size_gb must be at least 30."
  }
}

variable "network_cidr" {
  description = "CIDR for the dedicated Agent Desktop subnet."
  type        = string
  default     = "10.42.0.0/24"

  validation {
    condition     = can(cidrnetmask(var.network_cidr))
    error_message = "network_cidr must be a valid IPv4 CIDR."
  }
}

variable "deletion_protection" {
  description = "Protect the development VM from accidental Terraform deletion."
  type        = bool
  default     = false
}