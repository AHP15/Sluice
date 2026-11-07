import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "../../lib/utils";

const buttonVariants = cva(
  // D-031: 4px corners, hairline borders, 120ms state fades, and the accent
  // glow only on focus (`focus-glow`) — never at rest.
  "focus-glow inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-md border text-sm font-medium transition-[background-color,border-color,box-shadow,opacity] duration-interact disabled:pointer-events-none disabled:opacity-50",
  {
    variants: {
      variant: {
        // The primary action: the accent fill, used once per surface (D-032).
        default: "border-transparent bg-primary font-semibold text-primary-foreground hover:opacity-90",
        secondary: "border-border-strong bg-raised text-foreground hover:border-muted-foreground/50",
        outline: "border-input bg-transparent hover:bg-accent",
        ghost: "border-transparent text-muted-foreground hover:bg-accent hover:text-foreground",
        destructive: "border-transparent bg-destructive font-semibold text-destructive-foreground hover:opacity-90",
      },
      size: {
        // D-031 §5: compact.
        default: "h-8 px-3",
        sm: "h-7 px-2.5 text-xs",
        icon: "h-7 w-7",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {}

export const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, ...props }, ref) => (
    <button ref={ref} className={cn(buttonVariants({ variant, size }), className)} {...props} />
  ),
);
Button.displayName = "Button";

export { buttonVariants };
