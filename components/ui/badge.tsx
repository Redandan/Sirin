import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-semibold transition-colors",
  {
    variants: {
      variant: {
        default: "bg-slate-100 text-slate-800 dark:bg-slate-800 dark:text-slate-200",
        pending: "bg-amber-100 text-amber-800 dark:bg-amber-500/20 dark:text-amber-300",
        following: "bg-blue-100 text-blue-800 dark:bg-blue-500/20 dark:text-blue-300",
        followup: "bg-red-100 text-red-800 dark:bg-red-500/20 dark:text-red-300",
        done: "bg-emerald-100 text-emerald-800 dark:bg-emerald-500/20 dark:text-emerald-300",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  }
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return (
    <span className={cn(badgeVariants({ variant }), className)} {...props} />
  );
}

export { Badge, badgeVariants };
