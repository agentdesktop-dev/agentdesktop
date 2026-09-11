import { UsageReport, type UsageReportProps } from "@agentdesktop/ui";

export type UsageViewProps = Omit<
  UsageReportProps,
  "title" | "unavailableMessage"
>;

export function UsageView(props: UsageViewProps) {
  return (
    <div className="page-stack usage-page">
      <UsageReport {...props} />
    </div>
  );
}
