package enrollment

import "time"

type Principal struct {
	Issuer  string
	Subject string
}

type Enrollment struct {
	ID                   string    `json:"enrollment_id"`
	Status               string    `json:"status"`
	Issuer               string    `json:"issuer"`
	Subject              string    `json:"subject"`
	PublicKeyFingerprint string    `json:"public_key_fingerprint"`
	CreatedAt            time.Time `json:"created_at"`
}

type Issuance struct {
	EnrollmentID         string
	OrganizationID       string
	OrganizationIssuer   string
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

type Approval struct {
	EnrollmentID   string    `json:"enrollment_id"`
	Status         string    `json:"status"`
	DeviceID       string    `json:"device_id"`
	CertificatePEM string    `json:"certificate_chain_pem"`
	SerialNumber   string    `json:"serial_number"`
	NotBefore      time.Time `json:"not_before"`
	NotAfter       time.Time `json:"not_after"`
}
