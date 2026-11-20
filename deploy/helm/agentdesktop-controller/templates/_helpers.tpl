{{- define "agentdesktop-controller.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "agentdesktop-controller.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{- define "agentdesktop-controller.labels" -}}
app.kubernetes.io/name: {{ include "agentdesktop-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end }}

{{- define "agentdesktop-controller.gatewayListenerEnabled" -}}
{{- if and .Values.gatewayListener.enabled (or .Values.gatewayJwt .Values.existingConfigSecret) }}true{{- end }}
{{- end }}

{{- define "agentdesktop-controller.gatewayServiceName" -}}
{{- $fullname := include "agentdesktop-controller.fullname" . }}
{{- $name := printf "%s-gateway" ($fullname | trunc 55 | trimSuffix "-") }}
{{- if eq $name $fullname }}
{{- fail (printf "the gateway Service name %q equals the fleet Service name; choose a fullnameOverride that does not end in -gateway" $name) }}
{{- end }}
{{- $name }}
{{- end }}

{{- define "agentdesktop-controller.gatewayServicePort" -}}
{{- .Values.gatewayListener.service.port | default (ternary 443 80 .Values.gatewayListener.tls) }}
{{- end }}

{{- define "agentdesktop-controller.metricsServiceName" -}}
{{- $fullname := include "agentdesktop-controller.fullname" . }}
{{- $name := printf "%s-metrics" ($fullname | trunc 55 | trimSuffix "-") }}
{{- if eq $name $fullname }}
{{- fail (printf "the metrics Service name %q equals the fleet Service name; choose a fullnameOverride that does not end in -metrics" $name) }}
{{- end }}
{{- $name }}
{{- end }}

{{- define "agentdesktop-controller.selectorLabels" -}}
app.kubernetes.io/name: {{ include "agentdesktop-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
