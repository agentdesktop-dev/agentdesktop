package main

import (
	"context"
	"errors"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/api"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/auth"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/store/postgres"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/transport"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/migrations"
)

func main() {
	migrate := flag.Bool("migrate", false, "apply pending database migrations before startup")
	flag.Parse()

	databaseURL := required("DATABASE_URL")
	issuer := required("OAUTH_ISSUER")
	audience := required("OAUTH_AUDIENCE")
	scope := required("OAUTH_SCOPE")
	administratorScope := required("ADMIN_OAUTH_SCOPE")
	organizationID := required("ORGANIZATION_ID")
	organizationName := required("ORGANIZATION_NAME")
	caCertificatePath := required("CA_CERTIFICATE_PATH")
	caKeyPath := required("CA_PRIVATE_KEY_PATH")
	serverCertificatePath := required("SERVER_TLS_CERTIFICATE_PATH")
	serverKeyPath := required("SERVER_TLS_PRIVATE_KEY_PATH")
	trustDomain := required("MTLS_TRUST_DOMAIN")
	listenAddress := value("LISTEN_ADDRESS", "127.0.0.1:8090")
	certificateLifetime, err := time.ParseDuration(value("CLIENT_CERTIFICATE_LIFETIME", "24h"))
	if err != nil {
		log.Fatalf("invalid CLIENT_CERTIFICATE_LIFETIME: %v", err)
	}
	reconciliationInterval, err := time.ParseDuration(value("ISSUANCE_RECONCILIATION_INTERVAL", "1m"))
	if err != nil || reconciliationInterval <= 0 {
		log.Fatalf("invalid ISSUANCE_RECONCILIATION_INTERVAL: %v", err)
	}
	reconciliationGrace, err := time.ParseDuration(value("ISSUANCE_RECONCILIATION_GRACE", "5m"))
	if err != nil || reconciliationGrace <= 0 {
		log.Fatalf("invalid ISSUANCE_RECONCILIATION_GRACE: %v", err)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if *migrate {
		if err := migrations.Apply(ctx, databaseURL); err != nil {
			log.Fatal(err)
		}
	}
	store, err := postgres.Open(ctx, databaseURL)
	if err != nil {
		log.Fatal(err)
	}
	defer store.Close()
	if err := store.EnsureOrganization(ctx, organizationID, issuer, organizationName); err != nil {
		log.Fatal(err)
	}
	client := &http.Client{Timeout: 10 * time.Second}
	validator, err := auth.Discover(ctx, client, issuer, audience, scope)
	if err != nil {
		log.Fatal(err)
	}
	administratorValidator, err := auth.Discover(ctx, client, issuer, audience, administratorScope)
	if err != nil {
		log.Fatal(err)
	}
	certificateIssuer, err := ca.LoadX509Issuer(
		caCertificatePath,
		caKeyPath,
		trustDomain,
		certificateLifetime,
	)
	if err != nil {
		log.Fatal(err)
	}
	tlsConfig, err := transport.LoadServerTLSConfig(
		serverCertificatePath,
		serverKeyPath,
		caCertificatePath,
	)
	if err != nil {
		log.Fatal(err)
	}
	enrollmentService := enrollment.NewService(store, certificateIssuer)
	renewalService := renewal.NewService(store, certificateIssuer)
	handler := api.NewServer(
		validator,
		administratorValidator,
		enrollmentService,
		api.WithRenewal(validator, renewalService, trustDomain),
	)
	server := &http.Server{
		Addr:              listenAddress,
		Handler:           handler,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       15 * time.Second,
		WriteTimeout:      15 * time.Second,
		IdleTimeout:       60 * time.Second,
		TLSConfig:         tlsConfig,
	}

	go func() {
		<-ctx.Done()
		shutdownContext, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if err := server.Shutdown(shutdownContext); err != nil {
			log.Printf("server shutdown failed: %v", err)
		}
	}()
	go reconcileIssuance(ctx, enrollmentService, reconciliationInterval, reconciliationGrace)
	go reconcileRenewals(ctx, renewalService, reconciliationInterval, reconciliationGrace)
	log.Printf("enrollment server listening on %s", listenAddress)
	if err := server.ListenAndServeTLS("", ""); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}

func reconcileRenewals(
	ctx context.Context,
	service *renewal.Service,
	interval time.Duration,
	grace time.Duration,
) {
	reconcile := func() {
		completed, err := service.Reconcile(ctx, time.Now().UTC().Add(-grace), 100)
		if err != nil && ctx.Err() == nil {
			log.Printf("renewal reconciliation failed: %v", err)
		}
		if completed > 0 {
			log.Printf("reconciled %d interrupted certificate renewals", completed)
		}
	}
	reconcile()
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			reconcile()
		}
	}
}

func reconcileIssuance(
	ctx context.Context,
	service *enrollment.Service,
	interval time.Duration,
	grace time.Duration,
) {
	reconcile := func() {
		completed, err := service.Reconcile(ctx, time.Now().UTC().Add(-grace), 100)
		if err != nil && ctx.Err() == nil {
			log.Printf("issuance reconciliation failed: %v", err)
		}
		if completed > 0 {
			log.Printf("reconciled %d interrupted certificate issuances", completed)
		}
	}
	reconcile()
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			reconcile()
		}
	}
}

func required(name string) string {
	value := os.Getenv(name)
	if value == "" {
		log.Fatalf("required environment variable %s is unset", name)
	}
	return value
}

func value(name, fallback string) string {
	if configured := os.Getenv(name); configured != "" {
		return configured
	}
	return fallback
}
