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
