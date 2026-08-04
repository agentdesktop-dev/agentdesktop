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
	DeviceID             string
	CSRDER               []byte
	PublicKeyFingerprint string
}

type IssuedCertificate struct {
	ChainPEM     string
	NotAfter     time.Time
	NotBefore    time.Time
	SerialNumber string
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
