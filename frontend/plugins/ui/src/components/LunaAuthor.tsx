import React from "react";

import Avatar from "@mui/material/Avatar";
import Stack, { type StackProps } from "@mui/material/Stack";
import Tooltip from "@mui/material/Tooltip";
import Typography, { type TypographyProps } from "@mui/material/Typography";

import type { LunaAuthor } from "@luna/core";
import Box from "@mui/material/Box";

const AuthorName = React.memo((props: { name: string; prefix?: string } & TypographyProps) => (
	<Typography {...props} sx={{ fontWeight: 500, paddingTop: 0.2 }}>
		<Typography variant="caption" style={{ opacity: 0.7 }} children={props.prefix ?? "by "} />
		{props.name}
	</Typography>
));

export const LunaAuthorDisplay = React.memo((props: { author: LunaAuthor | string; prefix?: string } & StackProps) => {
	const { author, prefix } = props;
	if (typeof author === "string") return <Box paddingTop={0.75} children={<AuthorName name={author} />} />;
	return (
		<Tooltip title={`Visit ${author.name}'s profile`}>
			<Stack
				direction="row"
				spacing={1}
				alignItems="center"
				onClick={() => {
					try {
						const parsed = new URL(author.url);
						if (parsed.protocol === "https:" || (parsed.protocol === "http:" && /^(localhost|127\.0\.0\.1)(:|$)/.test(parsed.host))) {
							window.open(author.url, "_blank", "noopener,noreferrer");
						}
					} catch {
						// invalid URL - ignore
					}
				}}
				{...props}
				sx={{
					cursor: "pointer",
					...props.sx,
				}}
			>
				<AuthorName name={author.name} prefix={prefix} />
				{author.avatarUrl && (
					<Avatar
						src={author.avatarUrl}
						sx={{
							width: 28,
							height: 28,
						}}
					/>
				)}
			</Stack>
		</Tooltip>
	);
});
