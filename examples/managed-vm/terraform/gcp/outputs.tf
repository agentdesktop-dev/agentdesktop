output "project_id" {
  description = "Google Cloud project containing the deployment."
  value       = var.project_id
}

output "zone" {
  description = "Compute Engine zone containing the VM."
  value       = var.zone
}

output "instance_name" {
  description = "Compute Engine instance name."
  value       = google_compute_instance.managed.name
}

output "public_ip" {
  description = "Static public IPv4 address. Point public_host at this address when Cloud DNS is not managed here."
  value       = google_compute_address.managed.address
}

output "public_host" {
  description = "Hostname embedded in certificates, OAuth configuration, and client bootstrap."
  value       = local.public_host
}

output "admin_url" {
  description = "Agent Desktop Administration URL after deployment and CA trust setup."
  value       = "https://${local.public_host}:8090/admin/"
}