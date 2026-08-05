package renewal

import "time"

type Claim struct {
	ID                   string
	OrganizationID       string
	OrganizationIssuer   string
	DeviceID             string
	CSRDER               []byte
	PublicKeyFingerprint string
	StartedAt            time.Time
	Completed            *Certificate
}

type Certificate struct {
	ChainPEM     string    `json:"certificate_chain_pem"`
	NotAfter     time.Time `json:"not_after"`
	NotBefore    time.Time `json:"not_before"`
	SerialNumber string    `json:"serial_number"`
}

type Response struct {
	RenewalID            string      `json:"renewal_id"`
	Status               string      `json:"status"`
	DeviceID             string      `json:"device_id"`
	PublicKeyFingerprint string      `json:"public_key_fingerprint"`
	Certificate          Certificate `json:"certificate"`
}
