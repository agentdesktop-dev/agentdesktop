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

{{- define "agentdesktop-controller.selectorLabels" -}}
app.kubernetes.io/name: {{ include "agentdesktop-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "agentdesktop-controller.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "agentdesktop-controller.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- required "serviceAccount.name is required when serviceAccount.create is false" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "agentdesktop-controller.fleetConfigurationName" -}}
{{- default (printf "%s-fleet-configuration" (include "agentdesktop-controller.fullname" .)) .Values.fleetConfiguration.name | trunc 63 | trimSuffix "-" }}
{{- end }}
