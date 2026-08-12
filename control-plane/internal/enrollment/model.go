package enrollment

import "time"

type Principal struct {
	Issuer      string
	Subject     string
	DisplayName string
}

type Enrollment struct {
	ID                   string    `json:"enrollment_id"`
	Status               string    `json:"status"`
	Issuer               string    `json:"issuer"`
	Subject              string    `json:"subject"`
	DeviceName           string    `json:"device_name,omitempty"`
	PublicKeyFingerprint string    `json:"public_key_fingerprint"`
	CreatedAt            time.Time `json:"created_at"`
}

type Issuance struct {
	EnrollmentID         string
	OrganizationID       string
	OrganizationIssuer   string
	UserID               string
	DeviceID             string
	CSRDER               []byte
	PublicKeyFingerprint string
	StartedAt            time.Time
}

type IssuedCertificate struct {
	ChainPEM     string    `json:"certificate_chain_pem"`
	NotAfter     time.Time `json:"not_after"`
	NotBefore    time.Time `json:"not_before"`
	SerialNumber string    `json:"serial_number"`
}

type Status struct {
	EnrollmentID         string             `json:"enrollment_id"`
	Status               string             `json:"status"`
	PublicKeyFingerprint string             `json:"public_key_fingerprint"`
	CreatedAt            time.Time          `json:"created_at"`
	DeviceID             string             `json:"device_id,omitempty"`
	Certificate          *IssuedCertificate `json:"certificate,omitempty"`
}

type AdministrativeRecord struct {
	EnrollmentID         string    `json:"enrollment_id"`
	Status               string    `json:"status"`
	Subject              string    `json:"subject"`
	Username             string    `json:"username,omitempty"`
	DeviceName           string    `json:"device_name,omitempty"`
	PublicKeyFingerprint string    `json:"public_key_fingerprint"`
	CreatedAt            time.Time `json:"created_at"`
	UpdatedAt            time.Time `json:"updated_at"`
	DeviceID             string    `json:"device_id,omitempty"`
}

type DeviceRevocation struct {
	DeviceID  string    `json:"device_id"`
	Status    string    `json:"status"`
	RevokedAt time.Time `json:"revoked_at"`
}

type AdministrativeDevice struct {
	DeviceID                       string     `json:"device_id"`
	DeviceName                     string     `json:"device_name,omitempty"`
	Status                         string     `json:"status"`
	Subject                        string     `json:"subject"`
	Username                       string     `json:"username,omitempty"`
	CreatedAt                      time.Time  `json:"created_at"`
	RevokedAt                      *time.Time `json:"revoked_at"`
	CurrentCertificateSerialNumber *string    `json:"current_certificate_serial_number"`
	CurrentCertificateNotAfter     *time.Time `json:"current_certificate_not_after"`
	CertificateCount               int64      `json:"certificate_count"`
	RenewalCount                   int64      `json:"renewal_count"`
}

type FleetSummary struct {
	PendingEnrollments      int64     `json:"pending_enrollments"`
	IssuingEnrollments      int64     `json:"issuing_enrollments"`
	ApprovedEnrollments     int64     `json:"approved_enrollments"`
	RejectedEnrollments     int64     `json:"rejected_enrollments"`
	ActiveDevices           int64     `json:"active_devices"`
	RevokedDevices          int64     `json:"revoked_devices"`
	CertificatesExpiring24H int64     `json:"certificates_expiring_24h"`
	Renewals24H             int64     `json:"renewals_24h"`
	GeneratedAt             time.Time `json:"generated_at"`
}

type Approval struct {
	EnrollmentID   string    `json:"enrollment_id"`
	Status         string    `json:"status"`
	DeviceID       string    `json:"device_id"`
	CertificatePEM string    `json:"certificate_chain_pem"`
	SerialNumber   string    `json:"serial_number"`
	NotBefore      time.Time `json:"not_before"`
	NotAfter       time.Time `json:"not_after"`
}
