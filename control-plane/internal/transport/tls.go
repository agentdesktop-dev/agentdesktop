package transport

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

func LoadServerTLSConfig(certificatePath, keyPath, clientCAPath string) (*tls.Config, error) {
	keyInfo, err := os.Lstat(filepath.Clean(keyPath))
	if err != nil {
		return nil, fmt.Errorf("inspect server TLS key: %w", err)
	}
	if !keyInfo.Mode().IsRegular() || keyInfo.Mode().Perm()&0o077 != 0 {
		return nil, errors.New("server TLS key must be a regular owner-only file")
	}
	certificate, err := tls.LoadX509KeyPair(filepath.Clean(certificatePath), filepath.Clean(keyPath))
	if err != nil {
		return nil, fmt.Errorf("load server TLS identity: %w", err)
	}
	clientCAPEM, err := os.ReadFile(filepath.Clean(clientCAPath))
	if err != nil {
		return nil, fmt.Errorf("read client certificate authority: %w", err)
	}
	clientCAs := x509.NewCertPool()
	if !clientCAs.AppendCertsFromPEM(clientCAPEM) {
		return nil, errors.New("client certificate authority file contains no certificates")
	}
	return &tls.Config{
		Certificates: []tls.Certificate{certificate},
		ClientAuth:   tls.VerifyClientCertIfGiven,
		ClientCAs:    clientCAs,
		MinVersion:   tls.VersionTLS13,
	}, nil
}
