export function CardHeader({
  title,
  description,
  action,
  heading: Heading = "h3",
}: {
  title: string;
  description: string;
  action?: React.ReactNode;
  heading?: "h2" | "h3";
}) {
  return (
    <div className="card-header">
      <div>
        <Heading>{title}</Heading>
        <p>{description}</p>
      </div>
      {action}
    </div>
  );
}
