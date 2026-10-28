import * as SelectPrimitive from "@radix-ui/react-select";
import { Check, ChevronDown, ChevronUp } from "lucide-react";
import { useId } from "react";

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

const EMPTY = "__self_agent_empty_option__";

export function Select({ value, options, onValueChange, disabled, id, className = "", title, "aria-label": ariaLabel }: {
  value: string; options: SelectOption[]; onValueChange: (value: string) => void;
  disabled?: boolean; id?: string; className?: string; title?: string; "aria-label"?: string;
}) {
  const generatedId = useId();
  return <SelectPrimitive.Root value={value || EMPTY} disabled={disabled}
    onValueChange={(next) => onValueChange(next === EMPTY ? "" : next)}>
    <SelectPrimitive.Trigger id={id ?? generatedId} className={`select-trigger ${className}`} title={title} aria-label={ariaLabel}>
      <SelectPrimitive.Value>{options.find((option) => option.value === value)?.label ?? value}</SelectPrimitive.Value>
      <SelectPrimitive.Icon className="select-chevron"><ChevronDown size={15} /></SelectPrimitive.Icon>
    </SelectPrimitive.Trigger>
    <SelectPrimitive.Portal>
      <SelectPrimitive.Content className="select-content" position="popper" sideOffset={6} collisionPadding={12}>
        <SelectPrimitive.ScrollUpButton className="select-scroll"><ChevronUp size={15} /></SelectPrimitive.ScrollUpButton>
        <SelectPrimitive.Viewport className="select-viewport">
          {options.map((option) => <SelectPrimitive.Item key={option.value} value={option.value || EMPTY}
            disabled={option.disabled} className="select-item">
            <SelectPrimitive.ItemText>{option.label}</SelectPrimitive.ItemText>
            <SelectPrimitive.ItemIndicator className="select-check"><Check size={15} /></SelectPrimitive.ItemIndicator>
          </SelectPrimitive.Item>)}
        </SelectPrimitive.Viewport>
        <SelectPrimitive.ScrollDownButton className="select-scroll"><ChevronDown size={15} /></SelectPrimitive.ScrollDownButton>
      </SelectPrimitive.Content>
    </SelectPrimitive.Portal>
  </SelectPrimitive.Root>;
}
