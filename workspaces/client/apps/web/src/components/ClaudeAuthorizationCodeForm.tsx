import { Button, Stack, TextInput } from "@mantine/core";

export function ClaudeAuthorizationCodeForm({
  value,
  onChange,
  onSubmit,
  disabled,
  loading,
  submitLabel,
}: {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  disabled: boolean;
  loading: boolean;
  submitLabel: string;
}) {
  return (
    <>
      <p>
        Complete any email verification on Claude’s sign-in page. Do not paste
        that email code here. After Claude signs you in, paste the final
        authorization response shown by Claude, including the code and state
        separated by #.
      </p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onSubmit();
        }}
      >
        <Stack gap="sm">
          <TextInput
            label="Final authorization response"
            description="Paste the complete value in code#state form."
            type="password"
            autoComplete="off"
            value={value}
            onChange={(event) => onChange(event.currentTarget.value)}
            disabled={disabled}
          />
          <Button
            type="submit"
            disabled={!value.trim() || disabled}
            loading={loading}
          >
            {submitLabel}
          </Button>
        </Stack>
      </form>
    </>
  );
}
