import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cx } from "./cx";

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md" | "lg";

const VARIANT: Record<Variant, string> = {
  // One accent fill in the whole interface, reserved for the primary action of
  // a view. Everything else is a border or a text change.
  primary: "bg-accent text-inverse hover:bg-accent-strong active:bg-accent-dim",
  secondary:
    "border border-line-strong bg-elevated text-secondary hover:border-line-stronger hover:bg-hover hover:text-primary active:bg-active",
  ghost: "text-muted hover:bg-hover hover:text-primary active:bg-active",
  danger:
    "border border-error/35 text-error hover:border-error/55 hover:bg-error/10 active:bg-error/15",
};

const SIZE: Record<Size, string> = {
  sm: "h-control-sm gap-1.5 px-2 text-xs",
  md: "h-control gap-2 px-2.5 text-sm",
  lg: "h-control-lg gap-2 px-3 text-base",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  icon?: ReactNode;
  iconRight?: ReactNode;
  block?: boolean;
}

export function Button({
  variant = "secondary",
  size = "md",
  icon,
  iconRight,
  block,
  className,
  children,
  type = "button",
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cx(
        "inline-flex shrink-0 items-center justify-center rounded-md font-medium",
        "transition-colors duration-fast ease-standard",
        "disabled:pointer-events-none disabled:opacity-40",
        VARIANT[variant],
        SIZE[size],
        block && "w-full",
        className,
      )}
      {...rest}
    >
      {icon}
      {children}
      {iconRight}
    </button>
  );
}

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** Required: an icon-only control needs an accessible name. */
  label: string;
  size?: "sm" | "md" | "lg";
  variant?: Variant;
  active?: boolean;
}

export function IconButton({
  label,
  size = "md",
  variant = "ghost",
  active,
  className,
  children,
  type = "button",
  ...rest
}: IconButtonProps) {
  const dimension =
    size === "sm" ? "size-control-sm" : size === "lg" ? "size-control-lg" : "size-control";
  return (
    <button
      type={type}
      aria-label={label}
      title={label}
      className={cx(
        "inline-flex shrink-0 items-center justify-center rounded-md",
        "transition-colors duration-fast ease-standard",
        "disabled:pointer-events-none disabled:opacity-40",
        dimension,
        VARIANT[variant],
        active && "bg-active text-primary",
        className,
      )}
      {...rest}
    >
      {children}
    </button>
  );
}
