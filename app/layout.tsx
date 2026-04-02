import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Sirin",
  description: "High-performance desktop AI agent",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
