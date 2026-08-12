locals {
  region      = replace(var.zone, "/-[a-z]$/", "")
  public_host = lower(var.public_host)
  required_services = toset(concat(
    ["compute.googleapis.com", "iap.googleapis.com"],
    var.dns_managed_zone == null ? [] : ["dns.googleapis.com"]
  ))
}

resource "google_project_service" "required" {
  for_each = local.required_services

  project            = var.project_id
  service            = each.value
  disable_on_destroy = false
}

resource "google_compute_network" "managed" {
  name                    = "${var.instance_name}-network"
  auto_create_subnetworks = false

  depends_on = [google_project_service.required]
}

resource "google_compute_subnetwork" "managed" {
  name          = "${var.instance_name}-subnet"
  ip_cidr_range = var.network_cidr
  region        = local.region
  network       = google_compute_network.managed.id
}

resource "google_compute_address" "managed" {
  name         = "${var.instance_name}-ip"
  region       = local.region
  network_tier = "PREMIUM"

  depends_on = [google_project_service.required]
}

resource "google_compute_firewall" "client_services" {
  name      = "${var.instance_name}-clients"
  network   = google_compute_network.managed.name
  direction = "INGRESS"
  priority  = 1000

  source_ranges = var.client_source_ranges
  target_tags   = ["agentdesktop-managed"]

  allow {
    protocol = "tcp"
    ports    = ["8090", "8443", "8444"]
  }
}

resource "google_compute_firewall" "iap_ssh" {
  name      = "${var.instance_name}-iap-ssh"
  network   = google_compute_network.managed.name
  direction = "INGRESS"
  priority  = 1000

  source_ranges = ["35.235.240.0/20"]
  target_tags   = ["agentdesktop-managed"]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

resource "google_compute_instance" "managed" {
  name                      = var.instance_name
  zone                      = var.zone
  machine_type              = var.machine_type
  allow_stopping_for_update = true
  deletion_protection       = var.deletion_protection
  tags                      = ["agentdesktop-managed"]

  boot_disk {
    initialize_params {
      image = "projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64"
      size  = var.boot_disk_size_gb
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.managed.id

    access_config {
      nat_ip       = google_compute_address.managed.address
      network_tier = "PREMIUM"
    }
  }

  metadata = {
    enable-oslogin         = "TRUE"
    block-project-ssh-keys = "TRUE"
  }

  metadata_startup_script = file("${path.module}/startup.sh.tftpl")

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  depends_on = [google_project_service.required]
}

resource "google_dns_record_set" "managed" {
  count = var.dns_managed_zone == null ? 0 : 1

  project      = var.project_id
  managed_zone = var.dns_managed_zone
  name         = "${local.public_host}."
  type         = "A"
  ttl          = 300
  rrdatas      = [google_compute_address.managed.address]

  depends_on = [google_project_service.required]
}