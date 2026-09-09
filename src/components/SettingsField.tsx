import type { ReactNode } from "react";

export function SettingsField({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <div>
      <div style={{ fontSize: 12, color: "#8a93a3", marginBottom: 4 }}>
        {label}
      </div>
      {children}
    </div>
  );
}
